use std::sync::Mutex;

// One process-wide counter shared by all temporary-file creation paths.
// Mutex is portable to targets without 64-bit atomic instruction support.
static TEMP_COUNTER: Mutex<u64> = Mutex::new(0);

pub(crate) fn next() -> u64 {
    let mut counter = TEMP_COUNTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let value = *counter;
    *counter = counter.wrapping_add(1);
    value
}
