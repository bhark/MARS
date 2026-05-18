//! MAP-level service metadata YAML writer.

use std::fmt::Write as _;

use super::skeleton::ServiceMetaSkeleton;
use super::yaml::yaml_quote;

/// Emit the optional `service.*` fields harvested from MAP-level METADATA.
/// Identity-shaped fields (`contact:` / `contact_email:`) sit at the top of
/// `service:`; OWS-shared metadata nests under `service.ows:`; WMS-only
/// metadata nests under `service.wms:`. Each block is emitted only when at
/// least one of its fields is set.
pub(super) fn write_service_meta(out: &mut String, svc: &ServiceMetaSkeleton) {
    if svc.has_structured_contact() || svc.contact_email.is_some() {
        let _ = writeln!(out, "  contact:");
        if let Some(v) = &svc.contact_person {
            let _ = writeln!(out, "    person: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_position {
            let _ = writeln!(out, "    position: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_organization {
            let _ = writeln!(out, "    organization: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_phone {
            let _ = writeln!(out, "    phone: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_fax {
            let _ = writeln!(out, "    fax: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_email {
            let _ = writeln!(out, "    email: {}", yaml_quote(v));
        }
        let any_addr = svc.address_type.is_some()
            || svc.address_street.is_some()
            || svc.address_city.is_some()
            || svc.address_state.is_some()
            || svc.address_postcode.is_some()
            || svc.address_country.is_some();
        if any_addr {
            let _ = writeln!(out, "    address:");
            if let Some(v) = &svc.address_type {
                let _ = writeln!(out, "      type: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_street {
                let _ = writeln!(out, "      street: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_city {
                let _ = writeln!(out, "      city: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_state {
                let _ = writeln!(out, "      state_or_province: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_postcode {
                let _ = writeln!(out, "      postcode: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_country {
                let _ = writeln!(out, "      country: {}", yaml_quote(v));
            }
        }
    }
    let any_ows = !svc.keywords.is_empty()
        || svc.online_resource.is_some()
        || svc.fees.is_some()
        || svc.access_constraints.is_some()
        || svc.encoding.is_some();
    if any_ows {
        let _ = writeln!(out, "  ows:");
        if !svc.keywords.is_empty() {
            let _ = writeln!(out, "    keywords:");
            for kw in &svc.keywords {
                let _ = writeln!(out, "      - {}", yaml_quote(kw));
            }
        }
        if let Some(v) = &svc.online_resource {
            let _ = writeln!(out, "    online_resource: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.fees {
            let _ = writeln!(out, "    fees: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.access_constraints {
            let _ = writeln!(out, "    access_constraints: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.encoding {
            let _ = writeln!(out, "    encoding: {}", yaml_quote(v));
        }
    }
    let any_fmt =
        !svc.getmap_formats.is_empty() || !svc.getfeatureinfo_formats.is_empty() || !svc.getlegend_formats.is_empty();
    let any_wms = svc.bbox_extended.is_some()
        || svc.sld_enabled.is_some()
        || !svc.advertised_crs.is_empty()
        || !svc.authorities.is_empty()
        || !svc.identifiers.is_empty()
        || any_fmt;
    if any_wms {
        let _ = writeln!(out, "  wms:");
        if let Some(b) = svc.bbox_extended {
            let _ = writeln!(out, "    bbox_extended: {b}");
        }
        if let Some(b) = svc.sld_enabled {
            let _ = writeln!(out, "    sld_enabled: {b}");
        }
        if !svc.advertised_crs.is_empty() {
            let _ = writeln!(out, "    advertised_crs:");
            for crs in &svc.advertised_crs {
                let _ = writeln!(out, "      - {}", yaml_quote(crs));
            }
        }
        if !svc.authorities.is_empty() {
            let _ = writeln!(out, "    authorities:");
            for (n, h) in &svc.authorities {
                let _ = writeln!(out, "      - name: {}", yaml_quote(n));
                let _ = writeln!(out, "        href: {}", yaml_quote(h));
            }
        }
        if !svc.identifiers.is_empty() {
            let _ = writeln!(out, "    identifiers:");
            for (a, v) in &svc.identifiers {
                let _ = writeln!(out, "      - authority: {}", yaml_quote(a));
                let _ = writeln!(out, "        value: {}", yaml_quote(v));
            }
        }
        if any_fmt {
            let _ = writeln!(out, "    formats:");
            if !svc.getmap_formats.is_empty() {
                let _ = writeln!(out, "      get_map:");
                for v in &svc.getmap_formats {
                    let _ = writeln!(out, "        - {}", yaml_quote(v));
                }
            }
            if !svc.getfeatureinfo_formats.is_empty() {
                let _ = writeln!(out, "      get_feature_info:");
                for v in &svc.getfeatureinfo_formats {
                    let _ = writeln!(out, "        - {}", yaml_quote(v));
                }
            }
            if !svc.getlegend_formats.is_empty() {
                let _ = writeln!(out, "      get_legend_graphic:");
                for v in &svc.getlegend_formats {
                    let _ = writeln!(out, "        - {}", yaml_quote(v));
                }
            }
        }
    }
}
