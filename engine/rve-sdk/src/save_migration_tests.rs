use crate::abi::{
    RVE_SAVE_MIGRATION_ABI_VERSION, RveParamEntry, RveParams, RveSaveMigration, RveStatus, RveStr,
    RveValue,
};
use crate::{Params, Value, run_save_migration};
use std::ffi::c_void;

#[derive(Default)]
struct Output {
    values: Params,
    error: Option<String>,
    reject: bool,
}

unsafe extern "C" fn set(ctx: *mut c_void, key: RveStr, value: *const RveValue) -> bool {
    let output = unsafe { &mut *ctx.cast::<Output>() };
    if output.reject {
        return false;
    }
    let key = unsafe { key.to_str() }.unwrap().to_owned();
    let value = unsafe { (*value).into_owned() }.unwrap();
    output.values.insert(key, value);
    true
}

unsafe extern "C" fn error(ctx: *mut c_void, message: RveStr) {
    let output = unsafe { &mut *ctx.cast::<Output>() };
    output.error = Some(unsafe { message.to_str() }.unwrap().to_owned());
}

fn run(
    abi_version: u32,
    output: &mut Output,
    migrate: impl FnOnce(u32, Params) -> Result<Params, String>,
) -> u32 {
    let entries = [RveParamEntry {
        key: RveStr::from_str_ref("points"),
        value: RveValue::integer(7),
    }];
    let migration = RveSaveMigration {
        abi_version,
        from_version: 3,
        values: RveParams {
            entries: entries.as_ptr(),
            len: entries.len(),
        },
        ctx: (output as *mut Output).cast(),
        set,
        error,
    };
    unsafe { run_save_migration(&migration, migrate) }
}

#[test]
fn migration_receives_owned_values_and_writes_the_complete_result() {
    let mut output = Output::default();
    let status = run(
        RVE_SAVE_MIGRATION_ABI_VERSION,
        &mut output,
        |from, mut values| {
            assert_eq!(from, 3);
            let points = values.remove("points").unwrap();
            values.insert("score".to_owned(), points);
            values.insert("tags".to_owned(), Value::Array(vec![Value::Bool(true)]));
            Ok(values)
        },
    );
    assert_eq!(status, RveStatus::Ok as u32);
    assert_eq!(
        output.values,
        Params::from([
            ("score".to_owned(), Value::Int(7)),
            ("tags".to_owned(), Value::Array(vec![Value::Bool(true)])),
        ])
    );
    assert_eq!(output.error, None);
}

#[test]
fn migration_errors_panics_rejections_and_unknown_layouts_fail_the_step() {
    let mut output = Output::default();
    let status = run(RVE_SAVE_MIGRATION_ABI_VERSION, &mut output, |_, _| {
        Err("unsupported layout".to_owned())
    });
    assert_eq!(status, RveStatus::Error as u32);
    assert_eq!(output.error.as_deref(), Some("unsupported layout"));

    let mut output = Output::default();
    let status = run(RVE_SAVE_MIGRATION_ABI_VERSION, &mut output, |_, _| {
        panic!("migration bug")
    });
    assert_eq!(status, RveStatus::Panicked as u32);
    assert!(output.error.is_some());

    let mut output = Output {
        reject: true,
        ..Output::default()
    };
    let status = run(RVE_SAVE_MIGRATION_ABI_VERSION, &mut output, |_, values| {
        Ok(values)
    });
    assert_eq!(status, RveStatus::Error as u32);

    let mut output = Output::default();
    let status = run(RVE_SAVE_MIGRATION_ABI_VERSION + 1, &mut output, |_, _| {
        unreachable!("an unknown layout must not reach the migration")
    });
    assert_eq!(status, RveStatus::Error as u32);
    assert!(
        unsafe { run_save_migration(std::ptr::null(), |_, values| Ok(values)) }
            == RveStatus::Error as u32
    );
}
