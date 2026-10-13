use std::error::Error;
use std::sync::Arc;

use super::Engines;
use crate::engine::container::ContainerRuntime;

#[cfg(unix)]
#[test]
fn runtime_and_engine_clones_share_the_application_registry_arc() -> Result<(), Box<dyn Error>> {
    let runtime = ContainerRuntime::docker();
    let first = runtime
        .launch_retention()
        .map_err(|_| "runtime registry unavailable")?;
    let second = runtime
        .launch_retention()
        .map_err(|_| "runtime registry unavailable on clone")?;
    assert!(Arc::ptr_eq(&first, &second));

    let root = tempfile::tempdir()?;
    let engines = Engines::for_tests(root.path());
    let from_bundle = engines
        .container_runtime
        .as_ref()
        .ok_or("container engine bundle omitted runtime")?
        .launch_retention()
        .map_err(|_| "engine bundle registry unavailable")?;
    let from_runtime = engines
        .container_runtime
        .as_ref()
        .ok_or("container runtime missing")?
        .launch_retention()
        .map_err(|_| "engine runtime registry unavailable")?;
    assert!(Arc::ptr_eq(&from_bundle, &from_runtime));
    let cloned = engines.clone();
    let from_clone = cloned
        .container_runtime
        .as_ref()
        .ok_or("cloned engine bundle omitted runtime")?
        .launch_retention()
        .map_err(|_| "cloned engine runtime registry unavailable")?;
    assert!(Arc::ptr_eq(&from_bundle, &from_clone));
    Ok(())
}
