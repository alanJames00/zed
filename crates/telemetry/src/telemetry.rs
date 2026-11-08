/// Records a telemetry event.
///
/// Telemetry collection has been removed from this build, so this macro discards the event.
/// It still type-checks its arguments, and references them inside a branch that never runs,
/// so call sites keep compiling and don't produce unused-variable warnings.
///
/// ```
/// # let url = "https://example.com";
/// telemetry::event!("Keymap Changed", version = "1.0.0");
/// telemetry::event!("Documentation Viewed", url, source = "Extension Upsell");
/// ```
#[macro_export]
macro_rules! event {
    ($name:expr) => {{
        if false {
            let _ = &$name;
        }
    }};
    ($name:expr, $($key:ident $(= $value:expr)?),+ $(,)?) => {{
        if false {
            let _ = &$name;
            $(
                let _ = &$crate::serialize_property!($key $(= $value)?);
            )+
        }
    }};
}

#[macro_export]
macro_rules! serialize_property {
    ($key:ident) => {
        $key
    };
    ($key:ident = $value:expr) => {
        $value
    };
}
