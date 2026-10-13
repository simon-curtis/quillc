use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};
use std::sync::OnceLock;

const DEFAULT_WORKERS: usize = 4;

/// Bounded work-stealing workers shared by independent compiler tasks.
/// The default pool uses up to four workers; `QUILL_JOBS` can tune that cap.
pub struct WorkPool {
    pool: ThreadPool,
}

impl WorkPool {
    pub fn new(worker_count: usize) -> Result<Self, ThreadPoolBuildError> {
        let pool = ThreadPoolBuilder::new()
            .num_threads(worker_count.max(1))
            .thread_name(|index| format!("quill-worker-{index}"))
            .build()?;
        Ok(Self { pool })
    }

    pub fn available() -> Result<Self, ThreadPoolBuildError> {
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        Self::new(worker_limit(workers))
    }

    /// Process-wide pool for the type-check phase. `QUILL_TYPECK_JOBS`
    /// overrides the frontend-wide `QUILL_JOBS` cap for isolated profiling.
    pub fn shared_typeck() -> Option<&'static Self> {
        static POOL: OnceLock<Option<WorkPool>> = OnceLock::new();
        POOL.get_or_init(|| {
            let available = std::thread::available_parallelism().map_or(1, usize::from);
            let requested = requested_jobs_from("QUILL_TYPECK_JOBS").or_else(requested_jobs);
            let workers = bounded_worker_count(usize::MAX, available, requested);
            Self::new(workers).ok()
        })
        .as_ref()
    }

    /// Avoid starting idle threads when a compilation phase has little work.
    pub fn for_tasks(task_count: usize) -> Result<Self, ThreadPoolBuildError> {
        Self::for_tasks_with_override(task_count, None)
    }

    /// Phase-specific worker override for profiling independently from the
    /// frontend-wide `QUILL_JOBS` cap.
    pub fn for_tasks_with_override(
        task_count: usize,
        override_variable: Option<&str>,
    ) -> Result<Self, ThreadPoolBuildError> {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        let requested = override_variable
            .and_then(requested_jobs_from)
            .or_else(requested_jobs);
        Self::new(bounded_worker_count(task_count, available, requested))
    }

    pub fn worker_count(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Run independent items in parallel and retain input order in the output.
    pub fn map<T, R, F>(&self, items: &[T], operation: F) -> Vec<R>
    where
        T: Sync,
        R: Send,
        F: Fn(&T) -> R + Send + Sync,
    {
        if items.len() <= 1 {
            return items.iter().map(operation).collect();
        }
        self.pool
            .install(|| items.par_iter().map(operation).collect())
    }

    /// Mutate independent items in parallel and retain their input order.
    pub fn map_mut<T, R, F>(&self, items: &mut [T], operation: F) -> Vec<R>
    where
        T: Send,
        R: Send,
        F: Fn(&mut T) -> R + Send + Sync,
    {
        if items.len() <= 1 {
            return items.iter_mut().map(operation).collect();
        }
        self.pool
            .install(|| items.par_iter_mut().map(operation).collect())
    }
}

fn worker_limit(available: usize) -> usize {
    bounded_worker_count(usize::MAX, available, requested_jobs())
}

fn requested_jobs() -> Option<usize> {
    requested_jobs_from("QUILL_JOBS")
}

fn requested_jobs_from(variable: &str) -> Option<usize> {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
}

fn bounded_worker_count(task_count: usize, available: usize, requested: Option<usize>) -> usize {
    let available = available.max(1);
    let worker_cap = requested
        .filter(|jobs| *jobs > 0)
        .map_or(DEFAULT_WORKERS.min(available), |jobs| jobs.min(available));
    task_count
        .max(1)
        .min(available)
        .min(worker_cap)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::WorkPool;

    #[test]
    fn maps_parallel_items_in_source_order() {
        let pool = WorkPool::new(4).unwrap();
        let input: Vec<_> = (0..1000).collect();
        let result = pool.map(&input, |value| value * 3);
        assert_eq!(
            result,
            input.iter().map(|value| value * 3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn caps_worker_count_to_the_available_task_count() {
        let pool = WorkPool::for_tasks(1).unwrap();
        assert_eq!(pool.worker_count(), 1);
    }

    #[test]
    fn worker_limit_caps_requested_jobs_to_available_cpus_and_tasks() {
        assert_eq!(super::bounded_worker_count(50, 12, None), 4);
        assert_eq!(super::bounded_worker_count(2, 12, None), 2);
        assert_eq!(super::bounded_worker_count(50, 12, Some(4)), 4);
        assert_eq!(super::bounded_worker_count(2, 12, Some(8)), 2);
        assert_eq!(super::bounded_worker_count(50, 12, Some(32)), 12);
        assert_eq!(super::bounded_worker_count(50, 12, Some(0)), 4);
    }

    #[test]
    fn maps_zero_or_one_item_without_parallel_dispatch() {
        let pool = WorkPool::new(4).unwrap();
        let empty: [usize; 0] = [];
        assert_eq!(pool.map(&empty, |value| value + 1), []);
        assert_eq!(pool.map(&[41], |value| value + 1), [42]);
    }

    #[test]
    fn mutates_parallel_items_and_retains_input_order() {
        let pool = WorkPool::new(4).unwrap();
        let mut values: Vec<_> = (0..1000).collect();
        let result = pool.map_mut(&mut values, |value| {
            *value *= 3;
            *value
        });
        assert_eq!(values, (0..1000).map(|value| value * 3).collect::<Vec<_>>());
        assert_eq!(result, values);
    }
}
