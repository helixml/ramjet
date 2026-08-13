use std::process::ExitCode;

use mini_dynamo::companion_attestation_provisioner::{
    CompanionAttestationProvisionerConfig, provision_authenticated_engine_incarnation,
};

fn main() -> ExitCode {
    if std::env::args_os().len() != 1 {
        eprintln!("engine attestation provisioning failed: invalid_arguments");
        return ExitCode::FAILURE;
    }
    let config = match CompanionAttestationProvisionerConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("engine attestation provisioning failed: {}", error.reason());
            return ExitCode::FAILURE;
        }
    };
    match provision_authenticated_engine_incarnation(&config) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engine attestation provisioning failed: {}", error.reason());
            ExitCode::FAILURE
        }
    }
}
