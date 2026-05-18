use crate::ConfigError;
use crate::model::ServiceMeta;

pub(super) fn validate_service(service: &ServiceMeta) -> Result<(), ConfigError> {
    let name = &service.name;
    if name.trim().is_empty() {
        return Err(ConfigError::Invalid("service.name must not be empty".into()));
    }
    if name.contains(' ') {
        return Err(ConfigError::Invalid(format!(
            "service.name {name:?} must not contain spaces"
        )));
    }
    let dpi = service.scale_dpi;
    if !dpi.is_finite() || dpi <= 0.0 {
        return Err(ConfigError::Invalid(format!(
            "service.scale_dpi must be a positive, finite number; got {dpi}"
        )));
    }
    Ok(())
}
