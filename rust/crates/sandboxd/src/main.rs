//! `agentkit-sandboxd`: the sandbox container's PID 1. Configuration comes
//! from the launcher's environment (`AGENTKIT_GENERATION`,
//! `AGENTKIT_IMAGE_DIGEST`, `AGENTKIT_CHANNEL_TOKEN`,
//! `AGENTKIT_CONTROL_SOCKET`, `AGENTKIT_EGRESS_SOCKET`; in a microVM guest
//! `AGENTKIT_CONTROL_VSOCK_PORT`, `AGENTKIT_EGRESS_VSOCK_PORT` in place of
//! the two sockets); the channel token is required and never logged.
//!
//! Exit status: 0 after `shutdown`; 1 when the control channel was lost;
//! 2 when it could not start (configuration, hardening, dial, listen,
//! accept, handshake); 70 when processes outlived a sweep.

use agentkit_sandboxd::{
    Config, ConfigError, Exit, ProcessMode, Transport, harden_process, run_with,
};

fn main() {
    let mode = ProcessMode::detect();
    if let Err(error) = harden_process(mode) {
        eprintln!("agentkit-sandboxd: cannot harden the process: {error}");
        std::process::exit(2);
    }
    let (config, transport) = match from_env() {
        Ok(read) => read,
        Err(error) => {
            eprintln!("agentkit-sandboxd: {error}");
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("agentkit-sandboxd: cannot start the runtime: {error}");
            std::process::exit(2);
        }
    };
    let code = match runtime.block_on(run_with(config, transport)) {
        Ok(Exit::Shutdown) => 0,
        Ok(Exit::ChannelLost) => 1,
        Err(error) => {
            eprintln!("agentkit-sandboxd: {error}");
            2
        }
    };
    // Exit at once: blocking tasks left on the runtime must not hold the
    // container up.
    std::process::exit(code);
}

fn from_env() -> Result<(Config, Transport), ConfigError> {
    Ok((Config::from_env()?, Transport::from_env()?))
}
