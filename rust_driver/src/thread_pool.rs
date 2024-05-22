use std::{sync::{atomic::AtomicBool, Arc}, thread::JoinHandle};

struct PollerThreadContext{
    thread : Option<JoinHandle<()>>,
    stop_flag : Arc<AtomicBool>,
    poller_name : String,
}

struct PollerThreadPool{
    workers: Vec<PollerThreadContext>,
}

impl PollerThreadPool{
    pub(crate) fn new() -> Self{
        PollerThreadPool{
            workers: Vec::new(),
        }
    }

    pub(crate) fn add_worker<CTX:Send+Sync,F:FnOnce(&CTX) + Send + 'static>(&mut self, poller: F,ctx : Arc<CTX>){
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();
        let thread = std::thread::spawn(move || {
            while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                poller(&ctx);
            }
        });
        self.workers.push(PollerThreadContext{
            thread: Some(thread),
            stop_flag: stop_flag_clone,
            poller_name: "Poller".to_string(),
        });
    }
}