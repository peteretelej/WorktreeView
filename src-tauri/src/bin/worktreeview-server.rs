// The headless server binary: everything lives in the lib's server module
// so the bin stays a launcher. The desktop's `run()` is never called and
// the `windows_subsystem` attribute stays desktop-only, so a server keeps
// its console for stderr.
fn main() {
    std::process::exit(worktreeview_lib::server::run_server());
}
