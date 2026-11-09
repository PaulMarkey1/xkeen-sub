pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const RESET: &str = "\x1b[0m";

pub fn success(message: &str) {
    println!("{GREEN}{message}{RESET}");
}

pub fn error(message: &str) {
    println!("{RED}{message}{RESET}");
}

pub fn warning(message: &str) {
    println!("{YELLOW}{message}{RESET}");
}
