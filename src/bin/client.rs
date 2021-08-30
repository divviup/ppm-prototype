use ::hpke::Serializable;
use color_eyre::eyre::Result;
use ppm_prototype::{
    hpke::Role,
    parameters::Parameters,
    upload::{EncryptedInputShare, Report},
};
use reqwest::Client;
use tracing::info;
use tracing_error::ErrorLayer;
use tracing_subscriber::{fmt, fmt::format::FmtSpan, layer::SubscriberExt, EnvFilter, Registry};

static CLIENT_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    "/",
    "client"
);

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("crypto error")]
    Crypto(#[from] ppm_prototype::hpke::Error),
    #[error("upload error")]
    Upload(#[from] ppm_prototype::upload::Error),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Pretty-print errors
    color_eyre::install()?;

    // Configure a tracing subscriber. The crate emits events using `info!`,
    // `err!`, etc. macros from crate `tracing`.
    let fmt_layer = fmt::layer()
        .with_span_events(FmtSpan::ENTER | FmtSpan::EXIT)
        .with_thread_ids(true)
        // TODO(timg): take an argument for pretty vs. full vs. compact output
        .pretty()
        .with_level(true)
        .with_target(true);

    let subscriber = Registry::default()
        .with(fmt_layer)
        // Configure filters with RUST_LOG env var. Format discussed at
        // https://docs.rs/tracing-subscriber/0.2.20/tracing_subscriber/filter/struct.EnvFilter.html
        .with(EnvFilter::from_default_env())
        .with(ErrorLayer::default());

    tracing::subscriber::set_global_default(subscriber)?;

    let main_span = tracing::span!(tracing::Level::INFO, "client main");
    let _enter = main_span.enter();

    let http_client = Client::builder().user_agent(CLIENT_USER_AGENT).build()?;

    let ppm_parameters = Parameters::fixed_parameters();

    let leader_hpke_config = ppm_parameters
        .hpke_config(Role::Leader, &http_client)
        .await?;

    let mut hpke_sender =
        leader_hpke_config.report_sender(&ppm_parameters.task_id(), Role::Leader)?;

    // TODO(timg): I don't like partially constructing the Report and then
    // filling in `encrypted_input_shares` later. Maybe impl Default on Report.
    let mut report = Report {
        task_id: ppm_parameters.task_id(),
        time: 1001,
        nonce: rand::random(),
        extensions: vec![],
        encrypted_input_shares: vec![],
    };

    let plaintext = "plaintext input share".as_bytes();
    let payload = hpke_sender.encrypt_input_share(&report, plaintext)?;
    report.encrypted_input_shares = vec![EncryptedInputShare {
        config_id: leader_hpke_config.id,
        encapsulated_context: hpke_sender.encapped_key.to_bytes().as_slice().to_vec(),
        payload,
    }];

    let upload_endpoint = ppm_parameters.leader_url.join("upload")?;

    let upload_status = http_client
        .post(upload_endpoint)
        .json(&report)
        .send()
        .await?
        .status();

    info!(?upload_status, "upload complete");

    Ok(())
}
