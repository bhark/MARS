use kube::core::Status;

use crate::map_kube_error;
use mars_definition_source::DefinitionSourceError;

fn status(code: u16, reason: &str) -> Status {
    Status::failure(&format!("test {reason}"), reason).with_code(code)
}

#[test]
fn maps_404_to_not_found() {
    let e = map_kube_error(kube::Error::Api(Box::new(status(404, "NotFound"))), "configmap ns/foo");
    match e {
        DefinitionSourceError::NotFound { what } => assert_eq!(what, "configmap ns/foo"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn maps_401_and_403_to_auth() {
    for code in [401_u16, 403] {
        let e = map_kube_error(
            kube::Error::Api(Box::new(status(code, "Forbidden"))),
            "configmap ns/foo",
        );
        match e {
            DefinitionSourceError::Auth { what } => {
                assert!(what.contains("configmap ns/foo"), "{what}");
                assert!(what.contains("Forbidden"), "{what}");
            }
            other => panic!("expected Auth for {code}, got {other:?}"),
        }
    }
}

#[test]
fn maps_other_api_error_to_network() {
    let e = map_kube_error(
        kube::Error::Api(Box::new(status(500, "InternalError"))),
        "configmap ns/foo",
    );
    match e {
        DefinitionSourceError::Network { what, .. } => assert_eq!(what, "configmap get"),
        other => panic!("expected Network, got {other:?}"),
    }
}
