//! THE SITE A PAGE RAN (sdk#399 step 4): exactly the 32-byte id in `/v1/contract/web/<link>/`, or nothing.

/// The controls are the forms a lenient decode turns into a well-formed WRONG id.
#[test]
fn the_site_a_page_ran_is_its_paths_link_exactly() {
    let id = [7u8; 32];
    let link = freenet_stdlib::prelude::ContractInstanceId::new(id).encode();
    for path in [format!("/v1/contract/web/{link}/"), format!("/v1/contract/web/{link}/index.html"), format!("/v1/contract/web/{link}")] {
        assert_eq!(page_io::site_id_of_path(&path), Some(id), "{path}");
    }
    for (what, path) in [
        ("a short link (from_base58 zero-pads it into the all-zero id)", "/v1/contract/web/1/".to_string()),
        ("not base58", "/v1/contract/web/0OIl/".to_string()),
        ("no link", "/v1/contract/web//".to_string()),
        ("another path", format!("/v1/contract/{link}/")),
        ("a dev server's page", "/index.html".to_string()),
        ("nothing", String::new()),
    ] {
        assert_eq!(page_io::site_id_of_path(&path), None, "{what} was taken for a site");
    }
}
