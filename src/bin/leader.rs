use color_eyre::eyre::{Context, Result};
use ppm_prototype::{hpke, leader::run_leader, parameters::Parameters, trace, Role};
use prio::vdaf::prio3::Prio3Aes128Sum;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    trace::install_subscriber();

    let ppm_parameters = Parameters::from_config_file().wrap_err("loading task parameters")?;
    let hpke_config =
        hpke::Config::from_config_file(Role::Leader).wrap_err("loading hpke config")?;
    let vdaf = Prio3Aes128Sum::new(2, 63).unwrap();

    let verify_param = ppm_parameters
        .decode_vdaf_verification_parameter(Role::Leader, &vdaf)
        .wrap_err("decoding VDAF verification parameter")?;

    run_leader(&ppm_parameters, &vdaf, &verify_param, &(), &hpke_config).await
}
