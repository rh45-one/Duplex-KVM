fn main() {
    let args: Vec<String> = std::env::args().collect();
    duplexd::run_daemon_with_args(&args);
}
