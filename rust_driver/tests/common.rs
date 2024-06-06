use log::{Level, LevelFilter, Metadata, Record, SetLoggerError};
use std::fs::OpenOptions;
use std::io::Write;

pub struct SimpleLogger {
    file: std::fs::File,
}

impl SimpleLogger {
    pub fn new(file_path: &str) -> SimpleLogger {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(file_path)
            .unwrap();
        SimpleLogger { file }
    }
}

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!("{} - {}", record.level(), record.args());
            writeln!(&self.file, "{} - {}", record.level(), record.args()).unwrap();
        }
    }

    fn flush(&self) {}
}

pub fn init_logging(file_path: &str) -> Result<(), SetLoggerError> {
    log::set_boxed_logger(Box::new(SimpleLogger::new(file_path)))
        .map(|()| log::set_max_level(LevelFilter::Debug))
}

#[macro_export]
macro_rules! setup_emulator {
    ($magic_virt_addr:expr, $heap_block_size:expr, $shm_path:expr, $script_path:expr, $script_file:expr) => {
        const ORDER: usize = 32;
        /// Use `LockedHeap` as global allocator
        #[global_allocator]
        static HEAP_ALLOCATOR: buddy_system_allocator::LockedHeap<ORDER> =
            buddy_system_allocator::LockedHeap::<ORDER>::new();
        static mut HEAP_START_ADDR: usize = 0;

        #[macro_use]
        extern crate ctor;

        #[ctor]
        fn init_global_allocator() {
            unsafe {
                libc::shm_unlink($shm_path.as_ptr() as *const libc::c_char);

                let shm_fd = libc::shm_open(
                    $shm_path.as_ptr() as *const libc::c_char,
                    libc::O_RDWR | libc::O_CREAT,
                    0o600,
                );
                assert!(shm_fd != -1, "shm_open failed");
                if libc::ftruncate(shm_fd, $heap_block_size as i64) == -1 {
                    libc::close(shm_fd);
                    return;
                }

                let heap = libc::mmap(
                    $magic_virt_addr as *mut std::ffi::c_void,
                    $heap_block_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    shm_fd,
                    0,
                );

                assert!(heap != libc::MAP_FAILED, "mmap failed");

                let addr = heap as usize;
                let size = $heap_block_size;
                HEAP_START_ADDR = addr;

                HEAP_ALLOCATOR.lock().init(addr, size);
            }

            // run simulator script
            let handle = std::process::Command::new("bash")
                .current_dir($script_path)
                .arg($script_file)
                .spawn()
                .expect("Failed to execute script");
            let output = handle.wait_with_output().expect("Failed to wait on child");
            if !output.status.success() {
                let s = String::from_utf8_lossy(&output.stderr);
                panic!("Failed to execute script: {}", s);
            } else {
                let s = String::from_utf8_lossy(&output.stdout);
                info!("script output: {}", s);
            }
        }

        #[dtor]
        fn cleanup_global_allocator() {
            unsafe {
                libc::munmap(HEAP_START_ADDR as *mut std::ffi::c_void, $heap_block_size);
                libc::shm_unlink($shm_path.as_ptr() as *const libc::c_char);
            }
        }
    };
}
