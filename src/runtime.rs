//! The tokio runtime for model requests and tools. GPUI's executors are for short
//! interface work; agent turns run here and report back over channels.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn get() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("crack")
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to start the async runtime")
    })
}

pub fn spawn<F>(task: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    get().spawn(task)
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    get().block_on(future)
}
