fn main() {
    let args: Vec<String> = std::env::args().collect();
    duplex_guard::run_guard_with_args(&args);
}
