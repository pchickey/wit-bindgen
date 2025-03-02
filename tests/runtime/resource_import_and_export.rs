use std::collections::HashMap;
use wasmtime::{component::Resource, Store};

use self::test::resource_import_and_export::test::{Host, HostThing, Thing};
use crate::resource_import_and_export::exports::test::resource_import_and_export::test::Test;

wasmtime::component::bindgen!(in "tests/runtime/resource_import_and_export");

#[derive(Default)]
pub struct MyHostThing {
    map_l: HashMap<u32, u32>,
    next_id: u32,
}

impl Host for MyHostThing {}

impl HostThing for MyHostThing {
    fn new(&mut self, v: u32) -> wasmtime::Result<wasmtime::component::Resource<Thing>> {
        let id = self.next_id;
        self.next_id += 1;
        self.map_l.insert(id, v + 1);
        Ok(Resource::new_own(id))
    }

    fn foo(&mut self, self_: wasmtime::component::Resource<Thing>) -> wasmtime::Result<u32> {
        let id = self_.rep();
        Ok(self.map_l[&id] + 2)
    }

    fn bar(&mut self, self_: wasmtime::component::Resource<Thing>, v: u32) -> wasmtime::Result<()> {
        let id = self_.rep();
        self.map_l.insert(id, v + 3);
        Ok(())
    }

    fn baz(
        &mut self,
        a: wasmtime::component::Resource<Thing>,
        b: wasmtime::component::Resource<Thing>,
    ) -> wasmtime::Result<wasmtime::component::Resource<Thing>> {
        let a = self.foo(a)?;
        let b = self.foo(b)?;
        self.new(a + b + 4)
    }

    fn drop(&mut self, rep: wasmtime::component::Resource<Thing>) -> wasmtime::Result<()> {
        let id = rep.rep();
        self.map_l.remove(&id);
        Ok(())
    }
}

#[test]
fn run() -> anyhow::Result<()> {
    crate::run_test(
        "resource_import_and_export",
        |linker| ResourceImportAndExport::add_to_linker(linker, |x| &mut x.0),
        |store, component, linker| {
            let (u, e) = ResourceImportAndExport::instantiate(store, component, linker)?;
            Ok((u.interface0, e))
        },
        run_test,
    )
}

fn run_test(instance: Test, store: &mut Store<crate::Wasi<MyHostThing>>) -> anyhow::Result<()> {
    let thing1 = instance.thing().call_constructor(&mut *store, 42)?;

    // 42 + 1 (constructor) + 1 (constructor) + 2 (foo) + 2 (foo)
    let foo1 = instance.thing().call_foo(&mut *store, thing1)?;
    assert_eq!(foo1, 48);

    // 33 + 3 (bar) + 3 (bar) + 2 (foo) + 2 (foo)
    instance.thing().call_bar(&mut *store, thing1, 33)?;
    let foo2 = instance.thing().call_foo(&mut *store, thing1)?;
    assert_eq!(foo2, 43);

    let thing2 = instance.thing().call_constructor(&mut *store, 81)?;
    let thing3 = instance.thing().call_baz(&mut *store, thing1, thing2)?;
    let foo3 = instance.thing().call_foo(&mut *store, thing3)?;
    assert_eq!(
        foo3,
        33 + 3 + 3 + 81 + 1 + 1 + 2 + 2 + 4 + 1 + 2 + 4 + 1 + 1 + 2 + 2
    );

    Ok(())
}
