use super::since_v0_8_0;
use crate::wasm_host::WasmState;
use gpui::BackgroundExecutor;
use semver::Version;
use std::{env, sync::OnceLock};
use wasmtime::component::Linker;

pub const MIN_VERSION: Version = Version::new(0, 9, 0);
pub const MAX_VERSION: Version = Version::new(0, 9, 0);

wasmtime::component::bindgen!({
    imports: {
        default: async | trappable,
    },
    path: "../extension_api/wit/since_v0.9.0",
    world: "platform-host",
});

use self::zed::extension::platform;

pub fn linker(executor: &BackgroundExecutor) -> &'static Linker<WasmState> {
    static LINKER: OnceLock<Linker<WasmState>> = OnceLock::new();
    LINKER.get_or_init(|| {
        super::new_linker(executor, |linker| {
            since_v0_8_0::Extension::add_to_linker::<_, WasmState>(linker, |state| state)?;
            // Version 0.9 changes only this import. Its exports and all other
            // imports retain the 0.8 types, so the existing export bindings apply.
            linker.allow_shadowing(true);
            let result = PlatformHost::add_to_linker::<_, WasmState>(linker, |state| state);
            linker.allow_shadowing(false);
            result
        })
    })
}

fn current_platform(
    operating_system: &str,
    architecture: &str,
) -> wasmtime::Result<(platform::Os, platform::Architecture)> {
    let operating_system = match operating_system {
        "macos" => platform::Os::Mac,
        "linux" => platform::Os::Linux,
        "windows" => platform::Os::Windows,
        "freebsd" => platform::Os::Freebsd,
        other => return Err(wasmtime::Error::msg(format!("unsupported os: {other}"))),
    };
    let architecture = match architecture {
        "aarch64" => platform::Architecture::Aarch64,
        "x86_64" => platform::Architecture::X8664,
        other => {
            return Err(wasmtime::Error::msg(format!(
                "unsupported architecture: {other}"
            )));
        }
    };
    Ok((operating_system, architecture))
}

impl platform::Host for WasmState {
    async fn current_platform(
        &mut self,
    ) -> wasmtime::Result<(platform::Os, platform::Architecture)> {
        current_platform(env::consts::OS, env::consts::ARCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use release_channel::ReleaseChannel;
    use wasm_encoder::{
        ComponentBuilder, ComponentTypeRef, ComponentValType, InstanceType, TypeBounds,
    };
    use wasmtime::{Engine, component::Component};

    #[test]
    fn test_current_platform() -> wasmtime::Result<()> {
        for (operating_system, expected) in [
            ("macos", platform::Os::Mac),
            ("linux", platform::Os::Linux),
            ("windows", platform::Os::Windows),
            ("freebsd", platform::Os::Freebsd),
        ] {
            for (architecture, expected_architecture) in [
                ("aarch64", platform::Architecture::Aarch64),
                ("x86_64", platform::Architecture::X8664),
            ] {
                assert_eq!(
                    current_platform(operating_system, architecture)?,
                    (expected, expected_architecture)
                );
            }
        }
        assert!(current_platform("unknown", "x86_64").is_err());
        assert!(current_platform("freebsd", "unknown").is_err());
        Ok(())
    }

    fn platform_component(
        engine: &Engine,
        operating_systems: &[&str],
    ) -> anyhow::Result<Component> {
        let mut instance = InstanceType::new();
        instance
            .ty()
            .defined_type()
            .enum_type(operating_systems.iter().copied());
        instance.export("os", ComponentTypeRef::Type(TypeBounds::Eq(0)));
        instance.ty().defined_type().enum_type(["aarch64", "x8664"]);
        instance.export("architecture", ComponentTypeRef::Type(TypeBounds::Eq(2)));
        instance
            .ty()
            .defined_type()
            .tuple([ComponentValType::Type(1), ComponentValType::Type(3)]);
        instance
            .ty()
            .function()
            .params::<_, ComponentValType>([])
            .result(Some(ComponentValType::Type(4)));
        instance.export("current-platform", ComponentTypeRef::Func(5));
        let mut component = ComponentBuilder::default();
        let instance_type = component.type_instance(None, &instance);
        component.import(
            "zed:extension/platform",
            ComponentTypeRef::Instance(instance_type),
        );
        Ok(Component::from_binary(engine, &component.finish())?)
    }

    #[gpui::test]
    fn test_platform_linker_compatibility(cx: &mut TestAppContext) -> anyhow::Result<()> {
        let new_linker = linker(cx.background_executor());
        let old_linker = since_v0_8_0::linker(cx.background_executor());
        let legacy_component =
            platform_component(old_linker.engine(), &["mac", "linux", "windows"])?;
        let freebsd_component =
            platform_component(new_linker.engine(), &["mac", "linux", "windows", "freebsd"])?;

        for legacy_linker in [
            super::super::since_v0_0_1::linker(cx.background_executor()),
            super::super::since_v0_0_4::linker(cx.background_executor()),
            super::super::since_v0_0_6::linker(cx.background_executor()),
            super::super::since_v0_1_0::linker(cx.background_executor()),
            super::super::since_v0_2_0::linker(cx.background_executor()),
            super::super::since_v0_3_0::linker(cx.background_executor()),
            super::super::since_v0_4_0::linker(cx.background_executor()),
            super::super::since_v0_5_0::linker(cx.background_executor()),
            super::super::since_v0_6_0::linker(cx.background_executor()),
            old_linker,
        ] {
            legacy_linker.instantiate_pre(&legacy_component)?;
            assert!(legacy_linker.instantiate_pre(&freebsd_component).is_err());
        }
        new_linker.instantiate_pre(&freebsd_component)?;
        assert!(new_linker.instantiate_pre(&legacy_component).is_err());
        Ok(())
    }

    #[test]
    fn test_platform_api_version_gating() {
        for channel in [ReleaseChannel::Dev, ReleaseChannel::Nightly] {
            assert!(super::super::is_supported_wasm_api_version(
                channel,
                MIN_VERSION
            ));
            assert!(super::super::is_supported_wasm_api_version(
                channel,
                Version::new(0, 8, 0)
            ));
        }
        for channel in [ReleaseChannel::Stable, ReleaseChannel::Preview] {
            assert!(!super::super::is_supported_wasm_api_version(
                channel,
                MIN_VERSION
            ));
            assert!(super::super::is_supported_wasm_api_version(
                channel,
                Version::new(0, 7, 0)
            ));
        }
    }
}
