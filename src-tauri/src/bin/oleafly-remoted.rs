//! Spike: headless command host. See `oleafly_lib::remote_spike`.
//!
//!   oleafly-remoted            stdio mode: lives as long as the link (ssh) does
//!   oleafly-remoted --attach   relay to the persistent daemon, starting it if needed
//!   oleafly-remoted --serve    the persistent daemon itself (started by --attach)
//!   oleafly-remoted --stop     stop the persistent daemon of this data folder
//!   oleafly-remoted --version  print the version and protocol, and exit
fn main() {
    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        Some("--version") => {
            println!("{}", oleafly_lib::remote_spike::version_line());
            Ok(())
        }
        Some("--attach") => oleafly_lib::remote_spike::run_attach(),
        Some("--serve") => oleafly_lib::remote_spike::run_serve(),
        Some("--stop") => oleafly_lib::remote_spike::run_stop(),
        None => oleafly_lib::remote_spike::run_stdio(),
        Some(other) => Err(format!("unknown option {other}")),
    };
    if let Err(error) = result {
        eprintln!("oleafly-remoted: {error}");
        std::process::exit(1);
    }
}
