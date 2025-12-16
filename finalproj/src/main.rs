use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Type Definitions 

pub type TaskId = u64;

/// Task priority levels determine execution order.
/// Numeric values (0, 1, 2) are used for array indexing convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    High = 0,
    Medium = 1,
    Low = 2,
}

impl Priority {
    /// Maps priority to queue array index.
    /// High=0 ensures it's checked first in standard iteration.
    fn as_usize(&self) -> usize {
        *self as usize
    }
    
    /// Returns all priorities in execution order (highest first).
    fn all() -> [Priority; 3] {
        [Priority::High, Priority::Medium, Priority::Low]
    }
}

/// Tracks metadata about a task's lifecycle and retry attempts.
#[derive(Debug, Clone)]
pub struct TaskMetadata {
    pub creation_time: Instant,
    pub retry_count: usize,
    pub max_retries: usize,
}

pub struct Task {
    pub id: TaskId,
    pub priority: Priority,
    pub payload: Box<dyn TaskPayload>,
    pub metadata: TaskMetadata,
}

/// Core trait for work that can be submitted to the queue.

/// static bound: Ensures the payload doesn't hold references to temporary stack data
/// that might become invalid once the task moves to a worker thread.

/// Required because payloads must cross thread boundaries safely.
pub trait TaskPayload: Send + 'static {
    /// Execute the work. Takes &mut self to allow internal state modification.
    fn execute(&mut self) -> Result<TaskResult, TaskError>;
    
    /// Optional cleanup callback if a task is cancelled before execution.
    fn cancel(&mut self) -> Result<(), TaskError> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum TaskResult {
    Success,
    // Can carry data if needed
}

#[derive(Debug, Clone)]
pub enum TaskError {
    Cancelled,
    ExecutionFailed(String),
    Panic(String),
}

/// Metrics 

#[derive(Default)]
struct Metrics {
    tasks_submitted: AtomicU64,
    tasks_completed: AtomicU64,
    tasks_failed: AtomicU64,
    tasks_cancelled: AtomicU64,
    tasks_retried: AtomicU64,
    tasks_dead_lettered: AtomicU64,
}

/// Shared Internal State 

struct FailedTaskInfo {
    id: TaskId,
    error: String,
    attempts: usize,
}

struct WorkerHealthData {
    last_heartbeat: Instant,
    tasks_completed: u64,
}

/// The shared mutable state for the entire work queue system.
/// Wrapped in Arc for thread-safe sharing across workers and the main queue handle.
struct SharedState {
    /// Priority-based task storage: Vec<VecDeque> instead of BinaryHeap.
    ///  We need FIFO ordering WITHIN each priority level, but BinaryHeap
    /// doesn't guarantee stable ordering for equal-priority items.
    /// Index mapping: 0=High, 1=Medium, 2=Low.
    /// 
    /// This is the "Big Lock" - main contention point under high load.
    /// Protected by a single Mutex to keep lock nesting simple and avoid deadlocks.
    queues: Mutex<Vec<VecDeque<Task>>>,
    
    /// Wakes sleeping workers when new work arrives or shutdown is signaled.
    /// Standard Condvar pattern: workers call wait() while holding the lock,
    /// atomically releasing the lock while sleeping.
    condvar: Condvar,
    
    /// IDs marked for cancellation. Checked before task execution.
    /// Using HashSet for O(1) lookup during the cancellation check.
    cancelled_tasks: Mutex<HashSet<TaskId>>,
    
    /// Final destination for tasks that failed after max retries.
    dead_letter_queue: Mutex<Vec<FailedTaskInfo>>,
    
    /// Graceful shutdown signal. Using SeqCst ordering for immediate visibility.
    shutdown: AtomicBool,
    
    /// Performance metrics using atomic operations for lock-free updates.
    metrics: Metrics,
    
    /// Current active worker count 
    active_workers: AtomicUsize,
    
    /// Per-worker health data: last heartbeat and task completion count.
    worker_health: Mutex<HashMap<usize, WorkerHealthData>>,
}

impl SharedState {
    fn new() -> Self {
        // Initialize queues for each priority level
        let mut queues = Vec::new();
        for _ in 0..3 {
            queues.push(VecDeque::new());
        }

        Self {
            queues: Mutex::new(queues),
            condvar: Condvar::new(),
            cancelled_tasks: Mutex::new(HashSet::new()),
            dead_letter_queue: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            metrics: Metrics::default(),
            active_workers: AtomicUsize::new(0),
            worker_health: Mutex::new(HashMap::new()),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

///The Work Queue 

#[derive(Clone)]
pub struct WorkQueue {
    state: Arc<SharedState>,
    // Monotonically increasing ID generator
    next_task_id: Arc<AtomicU64>,
}

impl WorkQueue {
    pub fn new() -> Self {
        Self {
            state: Arc::new(SharedState::new()),
            next_task_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn submit(&self, priority: Priority, payload: Box<dyn TaskPayload>) -> TaskId {
        self.submit_with_retries(priority, payload, 0)
    }

    pub fn submit_with_retries(&self, priority: Priority, payload: Box<dyn TaskPayload>, max_retries: usize) -> TaskId {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let task = Task {
            id,
            priority,
            payload,
            metadata: TaskMetadata {
                creation_time: Instant::now(),
                retry_count: 0,
                max_retries,
            },
        };

        {
            let mut queues_guard = self.state.queues.lock().unwrap();
            queues_guard[priority.as_usize()].push_back(task);
            self.state.metrics.tasks_submitted.fetch_add(1, Ordering::Relaxed);
        }
        // Notify one waiting worker that work is available
        self.state.condvar.notify_one();
        id
    }

    pub fn cancel_task(&self, id: TaskId) {
        let mut cancelled_guard = self.state.cancelled_tasks.lock().unwrap();
        cancelled_guard.insert(id);
        // We don't remove from queue immediately, worker will check before execution.
    }

    pub fn shutdown(&self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        // Wake up everyone so they can check the shutdown flag and exit
        self.state.condvar.notify_all();
    }

    /// Monitoring APIs

    /// Returns the number of tasks currently queued across all priority levels.
    pub fn queue_size(&self) -> usize {
        let guard = self.state.queues.lock().unwrap();
        guard.iter().map(|q| q.len()).sum()
    }

    /// Snapshot of all performance metrics.
    ///  These atomic reads might not be perfectly consistent with each other
    /// Returns: (submitted, completed, failed, cancelled, retried, dead_lettered)
    pub fn metrics_snapshot(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.state.metrics.tasks_submitted.load(Ordering::Relaxed),
            self.state.metrics.tasks_completed.load(Ordering::Relaxed),
            self.state.metrics.tasks_failed.load(Ordering::Relaxed),
            self.state.metrics.tasks_cancelled.load(Ordering::Relaxed),
            self.state.metrics.tasks_retried.load(Ordering::Relaxed),
            self.state.metrics.tasks_dead_lettered.load(Ordering::Relaxed),
        )
    }

    pub fn active_worker_count(&self) -> usize {
        self.state.active_workers.load(Ordering::Relaxed)
    }

    pub fn dead_letter_queue_size(&self) -> usize {
        self.state.dead_letter_queue.lock().unwrap().len()
    }

    pub fn get_dead_letters(&self) -> Vec<(TaskId, String, usize)> {
        self.state.dead_letter_queue.lock().unwrap()
            .iter()
            .map(|info| (info.id, info.error.clone(), info.attempts))
            .collect()
    }

    pub fn worker_health_snapshot(&self) -> HashMap<usize, (Instant, u64)> {
        self.state.worker_health.lock().unwrap()
            .iter()
            .map(|(id, data)| (*id, (data.last_heartbeat, data.tasks_completed)))
            .collect()
    }
}

// --- Worker Management ---

pub struct WorkerManager {
    state: Arc<SharedState>,
    // Keep handles to join on shutdown 
    threads: Vec<thread::JoinHandle<()>>,
}

impl WorkerManager {
    pub fn new(queue: &WorkQueue, initial_workers: usize) -> Self {
        let mut manager = Self {
            state: queue.state.clone(),
            threads: Vec::new(),
        };
        for i in 0..initial_workers {
            manager.spawn_worker(i);
        }
        manager
    }

    fn spawn_worker(&mut self, id: usize) {
        let state = self.state.clone();
        let builder = thread::Builder::new().name(format!("worker-{}", id));

        let handle = builder.spawn(move || {
            Self::worker_loop(state, id);
        }).expect("Failed to spawn worker thread");

        self.threads.push(handle);
    }

    fn worker_loop(state: Arc<SharedState>, worker_id: usize) {
        state.active_workers.fetch_add(1, Ordering::Relaxed);
        
        // Initialize worker health data
        {
            let mut health = state.worker_health.lock().unwrap();
            health.insert(worker_id, WorkerHealthData {
                last_heartbeat: Instant::now(),
                tasks_completed: 0,
            });
        }

        loop {
            // 1. Acquire Lock and Wait for Work
            // This mutex guard is the critical section for all queue access.
            let task_opt;
            {
                let mut queues_guard = state.queues.lock().unwrap();

                loop {
                    // Check shutdown condition first.
                    if state.is_shutdown() {
                        // Graceful shutdown: Try to drain remaining work before quitting.
                        if let Some(task) = Self::pop_highest_priority(&mut queues_guard) {
                            task_opt = Some(task);
                            break; // Found work during shutdown, go process it.
                        } else {
                            // Queue empty AND shutdown flag set. Time to exit.
                            state.active_workers.fetch_sub(1, Ordering::Relaxed);
                            // Cleanup health entry
                            state.worker_health.lock().unwrap().remove(&worker_id);
                            return;
                        }
                    }

                    // Norm operation: Try to pop highest priority task.
                    if let Some(task) = Self::pop_highest_priority(&mut queues_guard) {
                        task_opt = Some(task);
                        break; // Found work, go process it.
                    }

                    // No work right now. Sleep until notified 
                    // IMP: condvar.wait() atomically drops the mutex lock when it sleeps
                    // and re-acquires it before returning. This is crucial for correctness.
                    queues_guard = state.condvar.wait(queues_guard).unwrap();
                }
            } // Lock is dropped here. We must not hold the lock while executing user code!

            // 2. Process Task 
            if let Some(mut task) = task_opt {
                Self::process_task(&state, &mut task, worker_id);
            }
        }
    }

    // Scans the priority queues from highest to lowest to find the next task.
    // Assumes the lock on `queues` is already held.
    // Returns the highest priority task available, maintaining FIFO within each level.
    fn pop_highest_priority(queues: &mut MutexGuard<Vec<VecDeque<Task>>>) -> Option<Task> {
        for priority in Priority::all() {
            if let Some(task) = queues[priority.as_usize()].pop_front() {
                return Some(task);
            }
        }
        None
    }

    // Core execution logic handles cancellation, panics, and failure tracking.
    // We catch panics to prevent the worker thread from dying, satisfying the
    // "worker restart on failure" requirement implicitly by keeping the loop alive.
    fn process_task(state: &Arc<SharedState>, task: &mut Task, worker_id: usize) {
        // Update heartbeat 
        {
            let mut health = state.worker_health.lock().unwrap();
            if let Some(data) = health.get_mut(&worker_id) {
                data.last_heartbeat = Instant::now();
            }
        }

        //  (late cancellation check).
        // This allows cancelled tasks to skip execution entirely.
        {
            let cancelled_guard = state.cancelled_tasks.lock().unwrap();
            if cancelled_guard.contains(&task.id) {
                // Give the payload a chance to clean up if needed.
                let _ = task.payload.cancel();
                state.metrics.tasks_cancelled.fetch_add(1, Ordering::Relaxed);
                // Cleanup the ID from the set to prevent infinite growth.
                drop(cancelled_guard);
                state.cancelled_tasks.lock().unwrap().remove(&task.id);
                return; // Skip execution.
            }
        }

        // Execute with Panic Catching using std::panic::catch_unwind.
        // This prevents the OS thread from dying if user code panics.
        // We rely on AssertUnwindSafe. If the payload leaves shared state corrupted
        // during a panic, future tasks might behave oddly, but the worker survives.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task.payload.execute()
        }));

        match result {
            Ok(Ok(_)) => {
                state.metrics.tasks_completed.fetch_add(1, Ordering::Relaxed);
                // Update completed count for health monitoring
                {
                    let mut health = state.worker_health.lock().unwrap();
                    if let Some(data) = health.get_mut(&worker_id) {
                        data.tasks_completed += 1;
                    }
                }
            }
            Ok(Err(task_err)) => {
                // Task ran without panicking but returned an error.
                state.metrics.tasks_failed.fetch_add(1, Ordering::Relaxed);
                // Send to dead letter queue 
                state.metrics.tasks_dead_lettered.fetch_add(1, Ordering::Relaxed);
                let mut dlq = state.dead_letter_queue.lock().unwrap();
                dlq.push(FailedTaskInfo {
                    id: task.id,
                    error: format!("Task error: {:?}", task_err),
                    attempts: task.metadata.retry_count + 1,
                });
            }
            Err(panic_err) => {
                // Task panicked. The worker thread survives because we caught it.
                state.metrics.tasks_failed.fetch_add(1, Ordering::Relaxed);
                // Try to get a meaningful message from the panic payload.
                let err_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                    format!("Panic: {}", s)
                } else if let Some(s) = panic_err.downcast_ref::<String>() {
                    format!("Panic: {}", s)
                } else {
                    "Panic with unknown type".to_string()
                };
                eprintln!("Worker {} caught panic in task {}: {}", worker_id, task.id, err_msg);
                
                // Send to dead letter queue.
                state.metrics.tasks_dead_lettered.fetch_add(1, Ordering::Relaxed);
                let mut dlq = state.dead_letter_queue.lock().unwrap();
                dlq.push(FailedTaskInfo {
                    id: task.id,
                    error: err_msg,
                    attempts: task.metadata.retry_count + 1,
                });
            }
        }
    }

    pub fn join_all(self) {
        for handle in self.threads {
            let _ = handle.join();
        }
    }
}


// Implementation Ex & Tests

// A simple task payload for testing
struct SimplePayload {
    duration: Duration,
    should_panic: bool,
    should_fail: bool,
}

impl TaskPayload for SimplePayload {
    fn execute(&mut self) -> Result<TaskResult, TaskError> {
        if self.should_panic {
            panic!("Simulated panic initiated!");
        }
        thread::sleep(self.duration);
        if self.should_fail {
            return Err(TaskError::ExecutionFailed("Simulated failure".to_string()));
        }
        Ok(TaskResult::Success)
    }
}

fn main() {
    println!("=== Work Queue Demonstration ===\n");
    
    let queue = WorkQueue::new();
    let worker_manager = WorkerManager::new(&queue, 4);

    println!("Phase 1: Submit mixed priority tasks");
    for _ in 0..5 {
        queue.submit(Priority::Low, Box::new(SimplePayload {
            duration: Duration::from_millis(10), should_panic: false, should_fail: false
        }));
    }

    for _ in 0..5 {
        queue.submit(Priority::High, Box::new(SimplePayload {
            duration: Duration::from_millis(10), should_panic: false, should_fail: false
        }));
    }

    println!("Phase 2: Submit task for cancellation");
    let cancel_id = queue.submit(Priority::Medium, Box::new(SimplePayload {
        duration: Duration::from_millis(500), should_panic: false, should_fail: false
    }));

    println!("Phase 3: Submit panicking task");
    queue.submit(Priority::Medium, Box::new(SimplePayload {
        duration: Duration::from_millis(10), should_panic: true, should_fail: false
    }));

    thread::sleep(Duration::from_millis(50));
    queue.cancel_task(cancel_id);
    println!("Cancelled task {}", cancel_id);

    thread::sleep(Duration::from_secs(1));

    println!("\n=== Metrics Snapshot ===");
    let (sub, comp, fail, canc, retry, dlq) = queue.metrics_snapshot();
    println!("Queue Size: {}", queue.queue_size());
    println!("Active Workers: {}", queue.active_worker_count());
    println!("Submitted: {}", sub);
    println!("Completed: {}", comp);
    println!("Failed: {}", fail);
    println!("Cancelled: {}", canc);
    println!("Retried: {}", retry);
    println!("Dead Letters: {}", dlq);

    println!("\n=== Worker Health ===");
    let health = queue.worker_health_snapshot();
    for (id, (_, count)) in health {
        println!("Worker {}: {} tasks completed", id, count);
    }

    if dlq > 0 {
        println!("\n=== Dead Letter Queue ===");
        for (id, err, attempts) in queue.get_dead_letters() {
            println!("Task {} (attempts: {}): {}", id, attempts, err);
        }
    }

    println!("\nShutting down...");
    queue.shutdown();
    worker_manager.join_all();
    println!("Shutdown complete.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    struct CountingPayload {
        counter: Arc<AtomicUsize>,
    }
    impl TaskPayload for CountingPayload {
        fn execute(&mut self) -> Result<TaskResult, TaskError> {
            self.counter.fetch_add(1, Ordering::Relaxed);
            Ok(TaskResult::Success)
        }
    }

    #[test]
    fn test_basic_submission_and_completion() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 2);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        thread::sleep(Duration::from_millis(200));
        queue.shutdown();
        _wm.join_all();

        assert_eq!(counter.load(Ordering::Relaxed), 10);
        let (_, comp, _, _, _, _) = queue.metrics_snapshot();
        assert_eq!(comp, 10);
    }

    #[test]
    fn test_priorities() {
        let queue = WorkQueue::new();
        let execution_order = Arc::new(Mutex::new(Vec::new()));

        struct TrackingPayload { id: u64, prio: Priority, list: Arc<Mutex<Vec<(u64, Priority)>>> }
        impl TaskPayload for TrackingPayload {
            fn execute(&mut self) -> Result<TaskResult, TaskError> {
                self.list.lock().unwrap().push((self.id, self.prio));
                Ok(TaskResult::Success)
            }
        }

        // Submit Low then High
        let id_low = queue.submit(Priority::Low, Box::new(TrackingPayload{ id: 1, prio: Priority::Low, list: execution_order.clone() }));
        let id_high = queue.submit(Priority::High, Box::new(TrackingPayload{ id: 2, prio: Priority::High, list: execution_order.clone() }));

        // Start 1 worker to guarantee execution order
        let _wm = WorkerManager::new(&queue, 1);
        thread::sleep(Duration::from_millis(100));
        queue.shutdown();
        _wm.join_all();

        let list = execution_order.lock().unwrap();
        assert_eq!(list.len(), 2);
        // High priority should start first even if submitted later
        assert_eq!(list[0], (id_high, Priority::High));
        assert_eq!(list[1], (id_low, Priority::Low));
    }

    #[test]
    fn test_worker_panic_recovery() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 1);
        let counter = Arc::new(AtomicUsize::new(0));

        // 1. Submit panicking task
        queue.submit(Priority::High, Box::new(SimplePayload{ duration: Duration::ZERO, should_panic: true, should_fail: false }));
        // 2. Submit normal task
        queue.submit(Priority::High, Box::new(CountingPayload{ counter: counter.clone() }));

        thread::sleep(Duration::from_millis(100));
        queue.shutdown();
        _wm.join_all();

        // The normal task should have completed despite the panic in the previous task
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        let (_, _, fail, _, _, _) = queue.metrics_snapshot();
        assert_eq!(fail, 1);
    }

    #[test]
    fn test_task_cancellation() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 2);
        let counter = Arc::new(AtomicUsize::new(0));

        let id = queue.submit(Priority::High, Box::new(CountingPayload{ counter: counter.clone() }));
        // Cancel before it might execute
        queue.cancel_task(id);

        thread::sleep(Duration::from_millis(50));
        queue.shutdown();
        _wm.join_all();

        let (_, _, _, canc, _, _) = queue.metrics_snapshot();
        assert!(canc >= 1, "Expected at least 1 cancelled task");
    }

    #[test]
    fn test_dead_letter_queue() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 1);

        // Send failing task
        queue.submit(Priority::High, Box::new(SimplePayload {
            duration: Duration::ZERO,
            should_panic: true,
            should_fail: false,
        }));

        thread::sleep(Duration::from_millis(100));
        queue.shutdown();
        _wm.join_all();

        let (_, _, fail, _, _, dlq) = queue.metrics_snapshot();
        assert_eq!(fail, 1);
        assert_eq!(dlq, 1);
        assert_eq!(queue.dead_letter_queue_size(), 1);
        
        let dead_letters = queue.get_dead_letters();
        assert_eq!(dead_letters.len(), 1);
        assert!(dead_letters[0].1.contains("Panic"));
    }

    #[test]
    fn test_stress_1000_tasks() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 4);
        let counter = Arc::new(AtomicUsize::new(0));

        // Submit 1000 tasks
        for _ in 0..1000 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        // Wait for completion
        thread::sleep(Duration::from_secs(2));
        queue.shutdown();
        _wm.join_all();

        assert_eq!(counter.load(Ordering::Relaxed), 1000);
        let (sub, comp, _, _, _, _) = queue.metrics_snapshot();
        assert_eq!(sub, 1000);
        assert_eq!(comp, 1000);
    }

    #[test]
    fn test_concurrent_workers_health() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 8);
        let counter = Arc::new(AtomicUsize::new(0));

        // Submit 80 tasks
        for _ in 0..80 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        thread::sleep(Duration::from_millis(500));
        
        // Check health snapshot before shutdown
        let health = queue.worker_health_snapshot();
        assert_eq!(health.len(), 8, "All 8 workers should be tracked");
        
        queue.shutdown();
        _wm.join_all();

        assert_eq!(counter.load(Ordering::Relaxed), 80);
    }

    #[test]
    fn test_mixed_priorities_execution() {
        let queue = WorkQueue::new();
        let execution_order = Arc::new(Mutex::new(Vec::new()));

        struct TrackingPayload { prio: Priority, list: Arc<Mutex<Vec<Priority>>> }
        impl TaskPayload for TrackingPayload {
            fn execute(&mut self) -> Result<TaskResult, TaskError> {
                self.list.lock().unwrap().push(self.prio);
                thread::sleep(Duration::from_millis(10));
                Ok(TaskResult::Success)
            }
        }

        // Submit in order: Medium, Low, High, Low, Medium, High
        queue.submit(Priority::Medium, Box::new(TrackingPayload{ prio: Priority::Medium, list: execution_order.clone() }));
        queue.submit(Priority::Low, Box::new(TrackingPayload{ prio: Priority::Low, list: execution_order.clone() }));
        queue.submit(Priority::High, Box::new(TrackingPayload{ prio: Priority::High, list: execution_order.clone() }));
        queue.submit(Priority::Low, Box::new(TrackingPayload{ prio: Priority::Low, list: execution_order.clone() }));
        queue.submit(Priority::Medium, Box::new(TrackingPayload{ prio: Priority::Medium, list: execution_order.clone() }));
        queue.submit(Priority::High, Box::new(TrackingPayload{ prio: Priority::High, list: execution_order.clone() }));

        let _wm = WorkerManager::new(&queue, 1);
        thread::sleep(Duration::from_millis(500));
        queue.shutdown();
        _wm.join_all();

        let list = execution_order.lock().unwrap();
        // All High priority should come first, then Medium, then Low
        let mut high_count = 0;
        let mut medium_start = None;
        let mut low_start = None;
        
        for (i, &prio) in list.iter().enumerate() {
            if prio == Priority::High {
                high_count += 1;
            } else if prio == Priority::Medium && medium_start.is_none() {
                medium_start = Some(i);
            } else if prio == Priority::Low && low_start.is_none() {
                low_start = Some(i);
            }
        }
        
        assert_eq!(high_count, 2, "Both high priority tasks should execute");
        assert!(medium_start.is_some() && low_start.is_some());
        assert!(high_count > 0);
    }

    #[test]
    fn test_queue_size_monitoring() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 1);
        let counter = Arc::new(AtomicUsize::new(0));

        // Submit 10 tasks
        for _ in 0..10 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        // Size should be around 10 (might be less if some already executing)
        let size = queue.queue_size();
        assert!(size <= 10 && size > 0, "Queue should have tasks pending");

        thread::sleep(Duration::from_secs(1));
        queue.shutdown();
        _wm.join_all();

        assert_eq!(queue.queue_size(), 0, "Queue should be empty after shutdown");
    }

    #[test]
    fn test_graceful_shutdown() {
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 2);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..20 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        // Let some process
        thread::sleep(Duration::from_millis(100));
        
        // Graceful shutdown
        queue.shutdown();
        _wm.join_all();

        // All queued tasks should have been processed
        let completed = counter.load(Ordering::Relaxed);
        assert_eq!(completed, 20, "All tasks should complete during graceful shutdown");
    }

    #[test]
    fn test_high_throughput() {
        let start = Instant::now();
        let queue = WorkQueue::new();
        let _wm = WorkerManager::new(&queue, 8);
        let counter = Arc::new(AtomicUsize::new(0));

        // Submit 5000 fast tasks
        for _ in 0..5000 {
            queue.submit(Priority::Medium, Box::new(CountingPayload{ counter: counter.clone() }));
        }

        thread::sleep(Duration::from_secs(3));
        queue.shutdown();
        _wm.join_all();

        let elapsed = start.elapsed();
        let completed = counter.load(Ordering::Relaxed);
        let throughput = completed as f64 / elapsed.as_secs_f64();
        
        println!("High throughput test: {:.0} tasks/sec (5000 tasks in {:?})", throughput, elapsed);
        assert_eq!(completed, 5000);
        // Should handle at least 1000 tasks/sec with 8 workers
        assert!(throughput >= 1000.0, "Throughput {} is below 1000 tasks/sec", throughput);
    }
}