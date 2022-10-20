wit_bindgen_guest_rust::generate!({
    import: "../../tests/runtime/results/imports.wit",
    default: "../../tests/runtime/results/exports.wit",
    name: "exports",
});

struct Exports;

export_exports!(Exports);

impl exports::Exports for Exports {
    fn string_error(a: f32) -> Result<f32, String> {
        imports::string_error(a)
    }
    fn enum_error(a: f64) -> Result<f64, exports::E> {
        match imports::enum_error(a) {
            Ok(b) => Ok(b),
            Err(imports::E::A) => Err(exports::E::A),
            Err(imports::E::B) => Err(exports::E::B),
            Err(imports::E::C) => Err(exports::E::C),
        }
    }
    fn empty_error(a: u32) -> Result<u32, ()> {
        imports::empty_error(a)
    }
}