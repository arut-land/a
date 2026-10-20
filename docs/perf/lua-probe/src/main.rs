fn main() {
    let home = std::env::args_os().nth(1).expect("isolated home");
    lua_probe::run(std::path::Path::new(&home));
}
