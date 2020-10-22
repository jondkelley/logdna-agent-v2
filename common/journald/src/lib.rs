pub mod source {
    use systemd::journal::{Journal, JournalFiles, JournalRecord, JournalSeek};
    use chrono::{Local, TimeZone};
    use http::types::body::LineBuilder;

    use log::{warn};

    use futures::stream::Stream;
    use std::{
        mem::drop,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            mpsc::{sync_channel, Receiver, TryRecvError},
            Arc,
            Mutex,
        },
        task::{Context, Poll, Waker},
        thread::{self, JoinHandle},
        time::UNIX_EPOCH,
    };

    const KEY_TRANSPORT: &str = "_TRANSPORT";
    const KEY_HOSTNAME: &str = "_HOSTNAME";
    const KEY_COMM: &str = "_COMM";
    const KEY_PID: &str = "_PID";
    const KEY_MESSAGE: &str = "MESSAGE";

    const TRANSPORT_AUDIT: &str = "audit";
    const TRANSPORT_DRIVER: &str = "driver";
    const TRANSPORT_SYSLOG: &str = "syslog";
    const TRANSPORT_JOURNAL: &str = "journal";
    const TRANSPORT_STDOUT: &str = "stdout";
    const TRANSPORT_KERNEL: &str = "kernel";

    #[derive(Clone)]
    pub enum JournalPath {
        Directory(PathBuf),
        Files(Vec<PathBuf>),
    }

    pub enum RecordStatus {
        Line(LineBuilder),
        BadLine,
        NoLines,
    }

    struct SharedState {
        waker: Option<Waker>,
    }

    pub struct JournaldStream {
        thread: Option<JoinHandle<()>>,
        receiver: Option<Receiver<LineBuilder>>,
        shared_state: Arc<Mutex<SharedState>>,
        path: JournalPath,
    }

    impl JournaldStream {
        pub fn new(path: JournalPath) -> Self {
            let mut stream = Self {
                thread: None,
                receiver: None,
                shared_state: Arc::new(Mutex::new(SharedState {
                    waker: None,
                })),
                path,
            };

            stream.spawn_thread();
            stream
        }

        fn spawn_thread(&mut self) {
            self.drop_thread();

            let (sender, receiver) = sync_channel(100);
            let thread_shared_state = self.shared_state.clone();
            let path = self.path.clone();
            let thread = thread::spawn(move || {
                let mut journal = JournaldSource::new(path);

                let call_waker = || {
                    let mut shared_state = match thread_shared_state.lock() {
                        Ok(shared_state) => shared_state,
                        Err(e) => {
                            // we can't wake up the stream so it will hang indefinitely; need
                            // to panic here
                            panic!("journald's worker thread unable to access shared state: {:?}", e);
                        }
                    };
                    if let Some(waker) = shared_state.waker.take() {
                        waker.wake();
                    }
                };

                loop {
                    if let RecordStatus::Line(line) = journal.process_next_record() {
                        if let Err(e) = sender.send(line) {
                            warn!("journald's worker thread unable to communicate with main thread: {}", e);
                            break;
                        }

                        call_waker();
                    } else {
                        match journal.reader.wait(None) {
                            Err(e) => {
                                warn!("journald's worker thread unable to poll journald for next record: {}", e);
                                break;
                            },
                            _ => {}
                        };
                    }
                }

                // some sort of error has occurred. Explicitly drop the sender before waking up the
                // stream to prevent a race condition
                drop(sender);
                call_waker();
            });

            self.thread = Some(thread);
            self.receiver = Some(receiver);
        }

        fn drop_thread(&mut self) {
            if let Some(thread) = self.thread.take() {
                if let Err(e) = thread.join() {
                    warn!("unable to join journald's worker thread: {:?}", e)
                }
            }
        }
    }

    impl Stream for JournaldStream {
        type Item = Vec<LineBuilder>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            let mut self_ = self.as_mut();

            if let Some(ref receiver) = self_.receiver {
                match receiver.try_recv() {
                    Ok(line) => {
                        return Poll::Ready(Some(vec![line]));
                    },
                    Err(TryRecvError::Disconnected) => {
                        warn!("journald's main thread unable to read from worker thread, restarting worker thread...");
                        self_.drop_thread();
                        self_.spawn_thread();
                    },
                    _ => {}
                }
            } else {
                warn!("journald's main thread missing connection to worker thread, shutting down stream");
                return Poll::Ready(None);
            }

            let mut shared_state = self_.shared_state.lock().unwrap();
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl Drop for JournaldStream {
        fn drop(&mut self) {
            self.drop_thread();
        }
    }

    pub struct JournaldSource {
        reader: Journal,
    }

    impl JournaldSource {
        pub fn new(path: JournalPath) -> JournaldSource {
            let mut reader = match path {
                JournalPath::Directory(path) => {
                    Journal::open_directory(&path, JournalFiles::All, false)
                        .expect("Could not open journald reader for directory")
                },
                JournalPath::Files(paths) => {
                    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                    Journal::open_files(&paths)
                        .expect("Could not open journald reader for paths")
                },
            };
            reader
                .seek(JournalSeek::Tail)
                .expect("Could not seek to tail of journald logs");

            JournaldSource { reader }
        }

        pub fn process_next_record(&mut self) -> RecordStatus {
            let record = match self.reader.next_entry() {
                Ok(Some(record)) => record,
                Ok(None) => return RecordStatus::NoLines,
                Err(e) => panic!("Unable to read next record from journald: {}", e),
            };

            let timestamp = match self.reader.timestamp() {
                Ok(timestamp) => Local
                    .timestamp(
                        timestamp.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64,
                        0,
                    )
                    .format("%b %d %H:%M:%S")
                    .to_string(),
                Err(e) => {
                    warn!(
                        "Unable to read timestamp associated with journald record: {}",
                        e
                    );
                    Local::now().format("%b %d %H:%M:%S").to_string()
                }
            };

            match record.get(KEY_TRANSPORT) {
                Some(transport) => match transport.as_ref() {
                    TRANSPORT_AUDIT => self.process_audit_record(&record, timestamp),
                    TRANSPORT_DRIVER | TRANSPORT_SYSLOG | TRANSPORT_JOURNAL | TRANSPORT_STDOUT => {
                        self.process_default_record(&record, transport, timestamp)
                    }
                    TRANSPORT_KERNEL => self.process_kernel_record(&record, timestamp),
                    _ => {
                        warn!(
                            "Got unexpected transport for journald record: {}",
                            transport
                        );
                        RecordStatus::BadLine
                    }
                },
                None => {
                    warn!("Unable to get transport of journald record");
                    RecordStatus::BadLine
                }
            }
        }

        fn process_audit_record(&self, record: &JournalRecord, timestamp: String) -> RecordStatus {
            let hostname = match record.get(KEY_HOSTNAME) {
                Some(hostname) => hostname,
                None => {
                    warn!("Unable to get hostname of journald audit record");
                    return RecordStatus::BadLine;
                }
            };

            let pid = match record.get(KEY_PID) {
                Some(pid) => pid,
                None => {
                    warn!("Unable to get pid of journald audit record");
                    return RecordStatus::BadLine;
                }
            };

            let message = match record.get(KEY_MESSAGE) {
                Some(message) => message,
                None => {
                    warn!("Unable to get message of journald audit record");
                    return RecordStatus::BadLine;
                }
            };

            RecordStatus::Line(LineBuilder::new().line(
                format!(
                    "{} {} audit[{}]: {}",
                    timestamp, hostname, pid, message
                )).file(hostname)
            )
        }

        fn process_default_record(
            &self,
            record: &JournalRecord,
            record_type: &String,
            timestamp: String,
        ) -> RecordStatus {
            let hostname = match record.get(KEY_HOSTNAME) {
                Some(hostname) => hostname,
                None => {
                    warn!("Unable to get hostname of journald {} record", record_type);
                    return RecordStatus::BadLine;
                }
            };

            let comm = match record.get(KEY_COMM) {
                Some(comm) => comm,
                None => {
                    warn!("Unable to get comm of journald {} record", record_type);
                    return RecordStatus::BadLine;
                }
            };

            let pid = match record.get(KEY_PID) {
                Some(pid) => pid,
                None => {
                    warn!("Unable to get pid of journald {} record", record_type);
                    return RecordStatus::BadLine;
                }
            };

            let message = match record.get(KEY_MESSAGE) {
                Some(message) => message,
                None => {
                    warn!("Unable to get message of journald {} record", record_type);
                    return RecordStatus::BadLine;
                }
            };

            RecordStatus::Line(LineBuilder::new().line(
                format!(
                    "{} {} {}[{}]: {}",
                    timestamp, hostname, comm, pid, message
                )).file(hostname)
            )
        }

        fn process_kernel_record(&self, record: &JournalRecord, timestamp: String) -> RecordStatus {
            let hostname = match record.get(KEY_HOSTNAME) {
                Some(hostname) => hostname,
                None => {
                    warn!("Unable to get hostname of journald kernel record");
                    return RecordStatus::BadLine;
                }
            };

            let message = match record.get(KEY_MESSAGE) {
                Some(message) => message,
                None => {
                    warn!("Unable to get message of journald kernel record");
                    return RecordStatus::BadLine;
                }
            };

            RecordStatus::Line(LineBuilder::new().line(
                format!("{} {} kernel: {}", timestamp, hostname, message)).file(hostname)
            )
        }
    }
}

mod tests {
    use tokio;

    #[tokio::test]
    async fn source_works() {
        let source = JournaldSource::new();
        let mut stream = source.into_stream().unwrap();
        while let Some(line) = stream.next().await {
            println!("{}", line);
        }
    }
}
