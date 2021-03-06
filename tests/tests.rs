use assert_matches::assert_matches;
use color_eyre::Result;
use http::StatusCode;
use ppm_prototype::{
    client::{self, PpmClient},
    collect::{self, run_collect},
    helper::run_helper,
    hpke,
    leader::run_leader,
    parameters::Parameters,
    trace, Duration, Interval, Time,
};
use prio::{
    field::Field128,
    vdaf::{
        prio3::{Prio3Aes128Sum, Prio3InputShare},
        Vdaf,
    },
};
use serial_test::serial;
use std::{io::Cursor, sync::Once};
use tokio::task::JoinHandle;

const INTERVAL_START: u64 = 1631907500;

// Install a trace subscriber once for all tests
static INSTALL_TRACE_SUBSCRIBER: Once = Once::new();

struct TestCase {
    parameters: Parameters,
    hpke_config: hpke::ConfigFile,
    client: PpmClient<Prio3Aes128Sum>,
    vdaf: Prio3Aes128Sum,
    leader_handle: JoinHandle<Result<()>>,
    helper_handle: JoinHandle<Result<()>>,
}

impl TestCase {
    async fn new_tamper(tamper_leader_proof: bool, tamper_helper_proof: bool) -> Self {
        INSTALL_TRACE_SUBSCRIBER.call_once(trace::install_subscriber);

        let parameters = Parameters::from_json_reader(Cursor::new(include_bytes!(
            "../sample-config/parameters.json"
        )))
        .unwrap();
        let hpke_config = hpke::ConfigFile::from_json_reader(Cursor::new(include_bytes!(
            "../sample-config/hpke.json"
        )))
        .unwrap();

        let leader_parameters = parameters.clone();
        let helper_parameters = parameters.clone();

        let vdaf = Prio3Aes128Sum::new(2, 63).unwrap();
        let leader_vdaf = vdaf.clone();
        let helper_vdaf = leader_vdaf.clone();
        let client_vdaf = leader_vdaf.clone();

        let leader_hpke_config = hpke_config.leader.clone();
        let helper_hpke_config = hpke_config.helper.clone();

        // Simulate negotation of verify parameter
        let (_, verify_parameters) = vdaf.setup().unwrap();

        let leader_verify_parameter = verify_parameters[0].clone();
        let helper_verify_parameter = verify_parameters[1].clone();

        // Spawn leader and helper tasks
        let leader_handle = tokio::spawn(async move {
            run_leader(
                &leader_parameters,
                &leader_vdaf,
                &leader_verify_parameter,
                &(),
                &leader_hpke_config,
            )
            .await
        });
        let helper_handle = tokio::spawn(async move {
            run_helper(
                &helper_parameters,
                &helper_vdaf,
                &helper_verify_parameter,
                &(),
                &helper_hpke_config,
            )
            .await
        });

        // Generate and upload 100 reports, with timestamps one second apart
        let client = PpmClient::new(&parameters, &client_vdaf, ()).await.unwrap();

        // libprio doesn't currently expose a way to tamper with input shares
        // (all fields of [`Prio3InputShare`] are private) so we neuter this
        // feature and the test cases that depend on it
        let tamper_leader_proof_func = if tamper_leader_proof {
            |input_share: &Prio3InputShare<Field128, 16>| input_share.clone()
        } else {
            |s: &Prio3InputShare<Field128, 16>| s.clone()
        };

        let tamper_helper_proof_func = if tamper_helper_proof {
            |input_share: &Prio3InputShare<Field128, 16>| input_share.clone()
        } else {
            |s: &Prio3InputShare<Field128, 16>| s.clone()
        };

        for count in 0..100 {
            client
                .do_upload_tamper(
                    INTERVAL_START + count,
                    &1,
                    &tamper_leader_proof_func
                        as &dyn Fn(&Prio3InputShare<Field128, 16>) -> Prio3InputShare<Field128, 16>,
                    &tamper_helper_proof_func
                        as &dyn Fn(&Prio3InputShare<Field128, 16>) -> Prio3InputShare<Field128, 16>,
                )
                .await
                .unwrap();
        }

        client.run_aggregate().await.unwrap();

        Self {
            parameters,
            hpke_config,
            client,
            vdaf,
            leader_handle,
            helper_handle,
        }
    }

    async fn new() -> Self {
        Self::new_tamper(false, false).await
    }

    async fn teardown(self) {
        // Kill leader and helper tasks
        self.leader_handle.abort();
        self.helper_handle.abort();

        assert!(self.leader_handle.await.unwrap_err().is_cancelled());
        assert!(self.helper_handle.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
#[serial]
async fn successful_aggregate() {
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    // The interval should capture all inputs send by client
    let collect_interval = Interval {
        start: Time(INTERVAL_START),
        duration: Duration(100),
    };

    // Successful collect
    let sum = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        collect_interval,
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap();

    assert_eq!(sum.0, 100);

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
async fn insufficient_batch_size() {
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    // Not enough inputs in the interval to meet min batch size
    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        Interval {
            start: Time(INTERVAL_START),
            duration: Duration(50),
        },
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:insufficientBatchSize".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
async fn exceed_privacy_budget() {
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    // The interval should capture all inputs send by client
    let collect_interval = Interval {
        start: Time(INTERVAL_START),
        duration: Duration(100),
    };

    // Successful collect
    let sum = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        collect_interval,
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap();

    assert_eq!(sum.0, 100);

    // Collect again over same interval. Should fail because privacy budget is
    // exceeded.
    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        collect_interval,
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:privacyBudgetExceeded".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
async fn unaligned_batch_interval() {
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        Interval {
            start: Time(INTERVAL_START),
            duration: Duration(99),
        },
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:invalidBatchInterval".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
async fn batch_interval_too_short() {
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        Interval {
            start: Time(INTERVAL_START),
            duration: Duration(25),
        },
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:invalidBatchInterval".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
#[ignore]
async fn invalid_helper_proof() {
    let test_case = TestCase::new_tamper(false, true).await;
    let aggregate_share_len = test_case.vdaf.output_len();

    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        Interval {
            start: Time(INTERVAL_START),
            duration: Duration(100),
        },
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        // There's no explicit error from proof rejection. Rather, those inputs
        // whose proofs were bad will simply be not have been aggregated, so
        // the collect request fails with insufficient batch size.
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:insufficientBatchSize".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
#[ignore]
async fn invalid_leader_proof() {
    let test_case = TestCase::new_tamper(true, false).await;
    let aggregate_share_len = test_case.vdaf.output_len();

    let error_document = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        Interval {
            start: Time(INTERVAL_START),
            duration: Duration(100),
        },
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap_err();

    assert_matches!(error_document, collect::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("collect".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        // There's no explicit error from proof rejection. Rather, those inputs
        // whose proofs were bad will simply be not have been aggregated, so
        // the collect request fails with insufficient batch size.
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:insufficientBatchSize".to_string()));
    });

    test_case.teardown().await;
}

#[tokio::test]
#[serial]
async fn report_uploaded_after_interval_collected() {
    // Successfully run aggregation over an interval
    let test_case = TestCase::new().await;
    let aggregate_share_len = test_case.vdaf.output_len();

    // The interval should capture all inputs send by client
    let collect_interval = Interval {
        start: Time(INTERVAL_START),
        duration: Duration(100),
    };

    // Successful collect
    let sum = run_collect(
        &test_case.parameters,
        &test_case.hpke_config.collector,
        collect_interval,
        test_case.vdaf.clone(),
        &(),
        aggregate_share_len,
    )
    .await
    .unwrap();

    assert_eq!(sum.0, 100);

    // Upload one more share, within the collected interval.
    let error_document = test_case
        .client
        .do_upload(INTERVAL_START, &1)
        .await
        .unwrap_err();

    assert_matches!(error_document, client::Error::ProblemDocument(problem_document) => {
        assert_eq!(problem_document.instance, Some("upload".to_string()));
        assert_eq!(problem_document.status, Some(StatusCode::BAD_REQUEST));
        assert_eq!(problem_document.type_url, Some("urn:ietf:params:ppm:error:staleReport".to_string()));
    });

    test_case.teardown().await;
}
