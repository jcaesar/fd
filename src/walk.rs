use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

use anyhow::{anyhow, Result};
use ignore::overrides::OverrideBuilder;
use ignore::{self, WalkBuilder};
use regex::bytes::Regex;

use crate::error::print_error;
use crate::exec;
use crate::exit_codes::{merge_exitcodes, ExitCode};
use crate::filesystem;
use crate::options::Options;
use crate::output;

/// The receiver thread can either be buffering results or directly streaming to the console.
enum ReceiverMode {
    /// Receiver is still buffering in order to sort the results, if the search finishes fast
    /// enough.
    Buffering,

    /// Receiver is directly printing results to the output.
    Streaming,
}

/// The Worker threads can result in a valid entry having PathBuf or an error.
pub enum WorkerResult {
    Entry(PathBuf),
    Error(ignore::Error),
}

/// Maximum size of the output buffer before flushing results to the console
pub const MAX_BUFFER_LENGTH: usize = 1000;

/// Recursively scan the given search path for files / pathnames matching the pattern.
///
/// If the `--exec` argument was supplied, this will create a thread pool for executing
/// jobs in parallel from a given command line and the discovered paths. Otherwise, each
/// path will simply be written to standard output.
pub fn scan(path_vec: &[PathBuf], pattern: Arc<Regex>, config: Arc<Options>) -> Result<ExitCode> {
    let mut path_iter = path_vec.iter();
    let first_path_buf = path_iter
        .next()
        .expect("Error: Path vector can not be empty");

    let mut override_builder = OverrideBuilder::new(first_path_buf.as_path());

    for pattern in &config.exclude_patterns {
        override_builder
            .add(pattern)
            .map_err(|e| anyhow!("Malformed exclude pattern: {}", e))?;
    }
    let overrides = override_builder
        .build()
        .map_err(|_| anyhow!("Mismatch in exclude patterns"))?;

    let mut walker = WalkBuilder::new(first_path_buf.as_path());
    walker
        .hidden(config.ignore_hidden)
        .ignore(config.read_fdignore)
        .parents(config.read_fdignore || config.read_vcsignore)
        .git_ignore(config.read_vcsignore)
        .git_global(config.read_vcsignore)
        .git_exclude(config.read_vcsignore)
        .overrides(overrides)
        .follow_links(config.follow_links)
        // No need to check for supported platforms, option is unavailable on unsupported ones
        .same_file_system(config.one_file_system)
        .max_depth(config.max_depth);

    if config.read_fdignore {
        walker.add_custom_ignore_filename(".fdignore");
    }

    if config.read_global_ignore {
        #[cfg(target_os = "macos")]
        let config_dir_op = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs_next::home_dir().map(|d| d.join(".config")));

        #[cfg(not(target_os = "macos"))]
        let config_dir_op = dirs_next::config_dir();

        if let Some(global_ignore_file) = config_dir_op
            .map(|p| p.join("fd").join("ignore"))
            .filter(|p| p.is_file())
        {
            let result = walker.add_ignore(global_ignore_file);
            match result {
                Some(ignore::Error::Partial(_)) => (),
                Some(err) => {
                    print_error(format!(
                        "Malformed pattern in global ignore file. {}.",
                        err.to_string()
                    ));
                }
                None => (),
            }
        }
    }

    for ignore_file in &config.ignore_files {
        let result = walker.add_ignore(ignore_file);
        match result {
            Some(ignore::Error::Partial(_)) => (),
            Some(err) => {
                print_error(format!(
                    "Malformed pattern in custom ignore file. {}.",
                    err.to_string()
                ));
            }
            None => (),
        }
    }

    for path_entry in path_iter {
        walker.add(path_entry.as_path());
    }

    let wants_to_quit = Arc::new(AtomicBool::new(false));

    // multithreaded
    #[cfg(not(target_os = "wasi"))]
    {
        let parallel_walker = walker.threads(config.threads).build_parallel();

        if config.ls_colors.is_some() && config.command.is_none() {
            let wq = Arc::clone(&wants_to_quit);
            ctrlc::set_handler(move || {
                if wq.load(Ordering::Relaxed) {
                    // Ctrl-C has been pressed twice, exit NOW
                    process::exit(ExitCode::KilledBySigint.into());
                } else {
                    wq.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        }

        let (tx, rx) = channel();

        // Spawn the thread that receives all results through the channel.
        let receiver_thread = spawn_receiver(&config, &wants_to_quit, rx);

        // Spawn the sender threads.
        spawn_senders(&config, &wants_to_quit, pattern, parallel_walker, tx);

        // Wait for the receiver thread to print out all results.
        let exit_code = receiver_thread.join().unwrap();

        if wants_to_quit.load(Ordering::Relaxed) {
            Ok(ExitCode::KilledBySigint)
        } else {
            Ok(exit_code)
        }
    }

    // possibly single-threaded
    #[cfg(target_os = "wasi")]
    {
        anyhow::ensure!(config.command.is_none(), "Executing commands not supported on WASI");
        let stdout = io::stdout();
        let mut acceptor = EntryPrinter::new(&config, &wants_to_quit, &stdout);
        let config_filter = config.clone();
        let pattern_filter = pattern.clone();
        walker.filter_entry(move |entry_o| {
            let (next, _) = filter_entry(&config_filter, &*pattern_filter, Ok(entry_o));
            next != ignore::WalkState::Skip
        });
        for entry_o in walker.build() {
                let (next, item) = match entry_o {
                    Ok(r) =>  filter_entry(&config, &*pattern, Ok(&r)),
                    Err(e) => filter_entry(&config, &*pattern, Err(e)),
                };
                if let Some(item) = item {
                    if !acceptor.accept(item) {
                        break;
                    }
                }
                if next == ignore::WalkState::Quit {
                    break;
                }
        }

        Ok(ExitCode::Success)
    }
}

fn spawn_receiver(
    config: &Arc<Options>,
    wants_to_quit: &Arc<AtomicBool>,
    rx: Receiver<WorkerResult>,
) -> thread::JoinHandle<ExitCode> {
    let config = Arc::clone(config);
    let wants_to_quit = Arc::clone(wants_to_quit);

    let threads = config.threads;

    thread::spawn(move || {
        let show_filesystem_errors = config.show_filesystem_errors;

        // This will be set to `Some` if the `--exec` argument was supplied.
        if let Some(ref cmd) = config.command {
            if cmd.in_batch_mode() {
                exec::batch(rx, cmd, show_filesystem_errors)
            } else {
                let shared_rx = Arc::new(Mutex::new(rx));

                let out_perm = Arc::new(Mutex::new(()));

                // Each spawned job will store it's thread handle in here.
                let mut handles = Vec::with_capacity(threads);
                for _ in 0..threads {
                    let rx = Arc::clone(&shared_rx);
                    let cmd = Arc::clone(cmd);
                    let out_perm = Arc::clone(&out_perm);

                    // Spawn a job thread that will listen for and execute inputs.
                    let handle =
                        thread::spawn(move || exec::job(rx, cmd, out_perm, show_filesystem_errors));

                    // Push the handle of the spawned thread into the vector for later joining.
                    handles.push(handle);
                }

                // Wait for all threads to exit before exiting the program.
                let mut results: Vec<ExitCode> = Vec::new();
                for h in handles {
                    results.push(h.join().unwrap());
                }

                merge_exitcodes(&results)
            }
        } else {
            let stdout = io::stdout();
            let mut acceptor = EntryPrinter::new(&config, &wants_to_quit, &stdout);

            for worker_result in rx {
                if !acceptor.accept(worker_result) {
                    break;
                }
            }

            ExitCode::Success
        }
    })
}

struct EntryPrinter<'a> {
    config: &'a Arc<Options>,
    start: time::Instant,
    mode: ReceiverMode,
    buffer: Vec<PathBuf>,
    num_results: usize,
    stdout: io::StdoutLock<'a>,
    wants_to_quit: &'a Arc<AtomicBool>,
}

impl<'a> EntryPrinter<'a> {
    fn new(config: &'a Arc<Options>, wants_to_quit: &'a Arc<AtomicBool>, stdout: &'a io::Stdout) -> EntryPrinter<'a> {
        EntryPrinter {
            config: config,
            start: time::Instant::now(),
            mode: ReceiverMode::Buffering,
            buffer: vec![],
            num_results: 0,
            stdout: stdout.lock(),
            wants_to_quit: wants_to_quit,
        }
    }

    fn accept(&mut self, worker_result: WorkerResult) -> bool {
        let show_filesystem_errors = self.config.show_filesystem_errors;
        // Maximum time to wait before we start streaming to the console.
        let max_buffer_time = self.config
            .max_buffer_time
            .unwrap_or_else(|| time::Duration::from_millis(100));
        match worker_result {
            WorkerResult::Entry(value) => {
                match self.mode {
                    ReceiverMode::Buffering => {
                        self.buffer.push(value);

                        // Have we reached the maximum buffer size or maximum buffering time?
                        if self.buffer.len() > MAX_BUFFER_LENGTH
                            || time::Instant::now() - self.start > max_buffer_time
                        {
                            // Flush the buffer
                            for v in &self.buffer {
                                output::print_entry(
                                    &mut self.stdout,
                                    v,
                                    self.config,
                                    self.wants_to_quit,
                                );
                            }
                            self.buffer.clear();

                            // Start streaming
                            self.mode = ReceiverMode::Streaming;
                        }
                    }
                    ReceiverMode::Streaming => {
                        output::print_entry(&mut self.stdout, &value, self.config, self.wants_to_quit);
                    }
                }

                self.num_results += 1;
            }
            WorkerResult::Error(err) => {
                if show_filesystem_errors {
                    print_error(err.to_string());
                }
            }
        }

        if let Some(max_results) = self.config.max_results {
            if self.num_results >= max_results {
                return false;
            }
        }

        true
    }
}

impl Drop for EntryPrinter<'_> {
    fn drop(&mut self) {
        // If we have finished fast enough (faster than max_buffer_time), we haven't streamed
        // anything to the console, yet. In this case, sort the results and print them:
        if !self.buffer.is_empty() {
            self.buffer.sort();
            for value in &self.buffer {
                output::print_entry(&mut self.stdout, value, self.config, self.wants_to_quit);
            }
        }
    }
}

pub enum DirEntry<'a> {
    Normal(&'a ignore::DirEntry),
    BrokenSymlink(PathBuf),
}

impl DirEntry<'_> {
    pub fn path(&self) -> &Path {
        match self {
            DirEntry::Normal(e) => e.path(),
            DirEntry::BrokenSymlink(pathbuf) => pathbuf.as_path(),
        }
    }

    pub fn file_type(&self) -> Option<FileType> {
        match self {
            DirEntry::Normal(e) => e.file_type(),
            DirEntry::BrokenSymlink(pathbuf) => {
                pathbuf.symlink_metadata().map(|m| m.file_type()).ok()
            }
        }
    }

    pub fn metadata(&self) -> Option<Metadata> {
        match self {
            DirEntry::Normal(e) => e.metadata().ok(),
            DirEntry::BrokenSymlink(_) => None,
        }
    }

    pub fn depth(&self) -> Option<usize> {
        match self {
            DirEntry::Normal(e) => Some(e.depth()),
            DirEntry::BrokenSymlink(_) => None,
        }
    }
}

fn spawn_senders(
    config: &Arc<Options>,
    wants_to_quit: &Arc<AtomicBool>,
    pattern: Arc<Regex>,
    parallel_walker: ignore::WalkParallel,
    tx: Sender<WorkerResult>,
) {
    parallel_walker.run(|| {
        let config = Arc::clone(config);
        let pattern = Arc::clone(&pattern);
        let tx_thread = tx.clone();
        let wants_to_quit = Arc::clone(wants_to_quit);

        Box::new(move |entry_o| {
            if wants_to_quit.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }

            let (next, item) = match entry_o {
                Ok(r) =>  filter_entry(&config, &*pattern, Ok(&r)),
                Err(e) => filter_entry(&config, &*pattern, Err(e)),
            };
            if let Some(item) = item {
                if tx_thread.send(item).is_err() {
                    return ignore::WalkState::Quit;
                }
            }

            next
        })
    });
}

fn filter_entry(
    config: &Arc<Options>,
    pattern: &Regex,
    entry_o: Result<&ignore::DirEntry, ignore::Error>,
) -> (ignore::WalkState, Option<WorkerResult>) {
    let empty_ok = (ignore::WalkState::Continue, None);

    let entry = match entry_o {
        Ok(ref e) if e.depth() == 0 => {
            // Skip the root directory entry.
            return empty_ok;
        }
        Ok(e) => DirEntry::Normal(e),
        Err(ignore::Error::WithPath {
            path,
            err: inner_err,
        }) => match inner_err.as_ref() {
            ignore::Error::Io(io_error)
                if io_error.kind() == io::ErrorKind::NotFound
                    && path
                        .symlink_metadata()
                        .ok()
                        .map_or(false, |m| m.file_type().is_symlink()) =>
            {
                DirEntry::BrokenSymlink(path)
            }
            _ => {
                return (ignore::WalkState::Continue, Some(WorkerResult::Error(ignore::Error::WithPath {
                    path,
                    err: inner_err,
                })))
            },
        },
        Err(err) => return (ignore::WalkState::Continue, Some(WorkerResult::Error(err))),
    };

    if let Some(min_depth) = config.min_depth {
        if entry.depth().map_or(true, |d| d < min_depth) {
            return empty_ok;
        }
    }

    // Check the name first, since it doesn't require metadata
    let entry_path = entry.path();

    let search_str: Cow<OsStr> = if config.search_full_path {
        let path_abs_buf = filesystem::path_absolute_form(entry_path)
            .expect("Retrieving absolute path succeeds");
        Cow::Owned(path_abs_buf.as_os_str().to_os_string())
    } else {
        match entry_path.file_name() {
            Some(filename) => Cow::Borrowed(filename),
            None => unreachable!(
                "Encountered file system entry without a file name. This should only \
                 happen for paths like 'foo/bar/..' or '/' which are not supposed to \
                 appear in a file system traversal."
            ),
        }
    };

    if !pattern.is_match(&filesystem::osstr_to_bytes(search_str.as_ref())) {
        return empty_ok;
    }

    // Filter out unwanted extensions.
    if let Some(ref exts_regex) = config.extensions {
        if let Some(path_str) = entry_path.file_name() {
            if !exts_regex.is_match(&filesystem::osstr_to_bytes(path_str)) {
                return empty_ok;
            }
        } else {
            return empty_ok;
        }
    }

    // Filter out unwanted file types.
    if let Some(ref file_types) = config.file_types {
        if let Some(ref entry_type) = entry.file_type() {
            if (!file_types.files && entry_type.is_file())
                || (!file_types.directories && entry_type.is_dir())
                || (!file_types.symlinks && entry_type.is_symlink())
                || (!file_types.sockets && filesystem::is_socket(entry_type))
                || (!file_types.pipes && filesystem::is_pipe(entry_type))
                || (file_types.executables_only
                    && !entry
                        .metadata()
                        .map(|m| filesystem::is_executable(&m))
                        .unwrap_or(false))
                || (file_types.empty_only && !filesystem::is_empty(&entry))
                || !(entry_type.is_file()
                    || entry_type.is_dir()
                    || entry_type.is_symlink()
                    || filesystem::is_socket(entry_type)
                    || filesystem::is_pipe(entry_type))
            {
                return empty_ok;
            }
        } else {
            return empty_ok;
        }
    }

    #[cfg(unix)]
    {
        if let Some(ref owner_constraint) = config.owner_constraint {
            if let Ok(ref metadata) = entry_path.metadata() {
                if !owner_constraint.matches(&metadata) {
                    return empty_ok;
                }
            } else {
                return empty_ok;
            }
        }
    }

    // Filter out unwanted sizes if it is a file and we have been given size constraints.
    if !config.size_constraints.is_empty() {
        if entry_path.is_file() {
            if let Ok(metadata) = entry_path.metadata() {
                let file_size = metadata.len();
                if config
                    .size_constraints
                    .iter()
                    .any(|sc| !sc.is_within(file_size))
                {
                    return empty_ok;
                }
            } else {
                return empty_ok;
            }
        } else {
            return empty_ok;
        }
    }

    // Filter out unwanted modification times
    if !config.time_constraints.is_empty() {
        let mut matched = false;
        if let Ok(metadata) = entry_path.metadata() {
            if let Ok(modified) = metadata.modified() {
                matched = config
                    .time_constraints
                    .iter()
                    .all(|tf| tf.applies_to(&modified));
            }
        }
        if !matched {
            return empty_ok;
        }
    }

    // Apply pruning.
    let skip = match config.prune {
        true => ignore::WalkState::Skip,
        false => ignore::WalkState::Continue,
    };

    (skip, Some(WorkerResult::Entry(entry_path.to_owned())))
}
