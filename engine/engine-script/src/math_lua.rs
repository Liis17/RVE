use glam::{EulerRot, Quat, Vec3};
use mlua::{AnyUserData, Lua, MetaMethod, Table, UserData, UserDataFields, UserDataMethods, Value};

pub(crate) fn install(lua: &Lua, rve: &Table) -> mlua::Result<()> {
    rve.set(
        "vec3",
        lua.create_function(|lua, (x, y, z): (f64, f64, f64)| {
            let value = Vec3::new(
                finite_f32(x, "x")?,
                finite_f32(y, "y")?,
                finite_f32(z, "z")?,
            );
            lua.create_userdata(Vec3UserData(value))
        })?,
    )?;
    rve.set(
        "quat",
        lua.create_function(|lua, value: AnyUserData| {
            let euler = value.borrow::<Vec3UserData>()?.0;
            lua.create_userdata(QuatUserData(Quat::from_euler(
                EulerRot::YXZ,
                euler.y.to_radians(),
                euler.x.to_radians(),
                euler.z.to_radians(),
            )))
        })?,
    )?;
    rve.set(
        "look_rotation",
        lua.create_function(|lua, (forward, up): (AnyUserData, AnyUserData)| {
            let forward = forward.borrow::<Vec3UserData>()?.0;
            let up = up.borrow::<Vec3UserData>()?.0;
            let direction = if forward.is_finite() && forward.length_squared() > 0.0 {
                forward.normalize()
            } else {
                return lua.create_userdata(QuatUserData(Quat::IDENTITY));
            };
            let up = if up.is_finite() && up.length_squared() > 0.0 {
                up.normalize()
            } else {
                Vec3::Y
            };
            let right = up.cross(direction).normalize_or_zero();
            let right = if right == Vec3::ZERO {
                if direction.y.abs() < 0.999 {
                    Vec3::Y.cross(direction).normalize()
                } else {
                    Vec3::X.cross(direction).normalize()
                }
            } else {
                right
            };
            let corrected_up = direction.cross(right).normalize_or_zero();
            lua.create_userdata(QuatUserData(Quat::from_mat3(&glam::Mat3::from_cols(
                right,
                corrected_up,
                direction,
            ))))
        })?,
    )?;
    rve.set(
        "quat_to_euler",
        lua.create_function(|lua, value: AnyUserData| {
            let q = value.borrow::<QuatUserData>()?.0;
            let (y, x, z) = q.to_euler(EulerRot::YXZ);
            lua.create_userdata(Vec3UserData(Vec3::new(
                x.to_degrees(),
                y.to_degrees(),
                z.to_degrees(),
            )))
        })?,
    )?;
    Ok(())
}

fn finite_f32(value: f64, name: &str) -> mlua::Result<f32> {
    let converted = value as f32;
    if value.is_finite() && converted.is_finite() {
        Ok(converted)
    } else {
        Err(mlua::Error::runtime(format!(
            "vec3.{name} must be a finite f32"
        )))
    }
}

fn number(value: Value, operation: &str) -> mlua::Result<f32> {
    let value = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => {
            return Err(mlua::Error::runtime(format!(
                "{operation} expects a number"
            )));
        }
    };
    finite_f32(value, operation)
}

fn vector(value: &AnyUserData, operation: &str) -> mlua::Result<Vec3> {
    value
        .borrow::<Vec3UserData>()
        .map(|value| value.0)
        .map_err(|_| mlua::Error::runtime(format!("{operation} expects vec3 userdata")))
}

fn quaternion(value: &AnyUserData, operation: &str) -> mlua::Result<Quat> {
    value
        .borrow::<QuatUserData>()
        .map(|value| value.0)
        .map_err(|_| mlua::Error::runtime(format!("{operation} expects quat userdata")))
}

struct Vec3UserData(Vec3);

impl UserData for Vec3UserData {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("x", |_, this| Ok(this.0.x));
        fields.add_field_method_get("y", |_, this| Ok(this.0.y));
        fields.add_field_method_get("z", |_, this| Ok(this.0.z));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("dot", |_, this, other: AnyUserData| {
            Ok(this.0.dot(vector(&other, "vec3.dot")?))
        });
        methods.add_method("cross", |lua, this, other: AnyUserData| {
            lua.create_userdata(Vec3UserData(this.0.cross(vector(&other, "vec3.cross")?)))
        });
        methods.add_method("normalize", |lua, this, ()| {
            lua.create_userdata(Vec3UserData(this.0.normalize_or_zero()))
        });
        methods.add_method("lerp", |lua, this, (other, t): (AnyUserData, f64)| {
            let t = finite_f32(t, "vec3.lerp t")?;
            lua.create_userdata(Vec3UserData(this.0.lerp(vector(&other, "vec3.lerp")?, t)))
        });
        methods.add_meta_function(
            MetaMethod::Add,
            |lua, (a, b): (AnyUserData, AnyUserData)| {
                lua.create_userdata(Vec3UserData(
                    vector(&a, "vec3 addition")? + vector(&b, "vec3 addition")?,
                ))
            },
        );
        methods.add_meta_function(
            MetaMethod::Sub,
            |lua, (a, b): (AnyUserData, AnyUserData)| {
                lua.create_userdata(Vec3UserData(
                    vector(&a, "vec3 subtraction")? - vector(&b, "vec3 subtraction")?,
                ))
            },
        );
        methods.add_meta_function(MetaMethod::Unm, |lua, value: AnyUserData| {
            lua.create_userdata(Vec3UserData(-vector(&value, "vec3 negation")?))
        });
        methods.add_meta_function(MetaMethod::Mul, |lua, (a, b): (Value, Value)| {
            let result = match (a, b) {
                (Value::UserData(a), Value::Number(b)) => {
                    vector(&a, "vec3 multiplication")? * number(Value::Number(b), "scalar")?
                }
                (Value::Number(a), Value::UserData(b)) => {
                    vector(&b, "vec3 multiplication")? * number(Value::Number(a), "scalar")?
                }
                (Value::UserData(a), Value::Integer(b)) => {
                    vector(&a, "vec3 multiplication")? * number(Value::Integer(b), "scalar")?
                }
                (Value::Integer(a), Value::UserData(b)) => {
                    vector(&b, "vec3 multiplication")? * number(Value::Integer(a), "scalar")?
                }
                _ => return Err(mlua::Error::runtime("vec3 multiplication expects a scalar")),
            };
            lua.create_userdata(Vec3UserData(result))
        });
    }
}

struct QuatUserData(Quat);

impl UserData for QuatUserData {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("x", |_, this| Ok(this.0.x));
        fields.add_field_method_get("y", |_, this| Ok(this.0.y));
        fields.add_field_method_get("z", |_, this| Ok(this.0.z));
        fields.add_field_method_get("w", |_, this| Ok(this.0.w));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("dot", |_, this, other: AnyUserData| {
            Ok(this.0.dot(quaternion(&other, "quat.dot")?))
        });
        methods.add_method("normalize", |lua, this, ()| {
            let length = this.0.length();
            let value = if length.is_finite() && length > 0.0 {
                this.0 / length
            } else {
                Quat::IDENTITY
            };
            lua.create_userdata(QuatUserData(value))
        });
        methods.add_method("lerp", |lua, this, (other, t): (AnyUserData, f64)| {
            let t = finite_f32(t, "quat.lerp t")?;
            lua.create_userdata(QuatUserData(
                this.0.lerp(quaternion(&other, "quat.lerp")?, t).normalize(),
            ))
        });
        methods.add_method("slerp", |lua, this, (other, t): (AnyUserData, f64)| {
            let t = finite_f32(t, "quat.slerp t")?;
            lua.create_userdata(QuatUserData(
                this.0
                    .slerp(quaternion(&other, "quat.slerp")?, t)
                    .normalize(),
            ))
        });
        methods.add_meta_function(
            MetaMethod::Mul,
            |lua, (a, b): (AnyUserData, AnyUserData)| {
                let result = if let Ok(a) = a.borrow::<QuatUserData>() {
                    if let Ok(b) = b.borrow::<QuatUserData>() {
                        Value::UserData(lua.create_userdata(QuatUserData(a.0 * b.0))?)
                    } else {
                        let value = vector(&b, "quat * vec3")?;
                        let rotated = a.0 * value;
                        Value::UserData(lua.create_userdata(Vec3UserData(rotated))?)
                    }
                } else {
                    return Err(mlua::Error::runtime(
                        "quat multiplication expects quat * quat or quat * vec3",
                    ));
                };
                Ok(result)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::install;
    use mlua::{Lua, Table};
    use rve_sdk::{Quat as SdkQuat, Vec3 as SdkVec3};

    #[test]
    fn lua_math_userdata_supports_basis_operations_and_finite_edges() {
        let lua = Lua::new();
        let rve = lua.create_table().unwrap();
        install(&lua, &rve).unwrap();
        lua.globals().set("rve", rve).unwrap();
        let output = lua
            .load(
                "local x=rve.vec3(1,0,0); local y=rve.vec3(0,1,0); \
                 local z=x:cross(y); local q=rve.quat(rve.vec3(0,90,0)); \
                 local e=rve.quat_to_euler(q); local look=rve.look_rotation(rve.vec3(0,0,1),rve.vec3(0,0,1)); \
                 return z.z, (q*rve.vec3(0,0,1)).x, e.y, look.w, rve.vec3(0,0,0):normalize().x",
            )
            .eval::<(f32, f32, f32, f32, f32)>()
            .unwrap();
        assert_eq!(output.0, 1.0);
        assert!((output.1 - 1.0).abs() < 1e-4, "rotated x was {}", output.1);
        assert!((output.2 - 90.0).abs() < 1e-4);
        assert!(output.3.is_finite());
        assert_eq!(output.4, 0.0);
    }

    #[test]
    fn lua_math_matches_the_std_only_rust_sdk_conventions() {
        let lua = Lua::new();
        let rve = lua.create_table().unwrap();
        install(&lua, &rve).unwrap();
        lua.globals().set("rve", rve).unwrap();
        let output: Table = lua
            .load(
                "local a=rve.vec3(2,-1,4); local b=rve.vec3(-3,5,1); \
                 local e=rve.vec3(25,-40,13); local q=rve.quat(e); \
                 local round=rve.quat_to_euler(q); \
                 local look=rve.look_rotation(rve.vec3(0,1,0),rve.vec3(0,1,0)); \
                 local half=rve.quat(rve.vec3(0,180,0)):slerp(rve.quat(rve.vec3(0,-180,0)),0.5); \
                 local cross=a:cross(b); \
                 return {cross={cross.x,cross.y,cross.z}, q={q.x,q.y,q.z,q.w}, \
                   round={round.x,round.y,round.z}, look={look.x,look.y,look.z,look.w}, \
                   half={half.x,half.y,half.z,half.w}}",
            )
            .eval()
            .unwrap();
        let sdk_a = SdkVec3::new(2.0, -1.0, 4.0);
        let sdk_b = SdkVec3::new(-3.0, 5.0, 1.0);
        let cross = output.get::<Table>("cross").unwrap();
        for (index, expected) in [
            sdk_a.cross(sdk_b).x,
            sdk_a.cross(sdk_b).y,
            sdk_a.cross(sdk_b).z,
        ]
        .into_iter()
        .enumerate()
        {
            assert!((cross.get::<f32>(index + 1).unwrap() - expected).abs() < 1.0e-4);
        }

        let sdk_q = SdkQuat::from_euler(SdkVec3::new(25.0, -40.0, 13.0));
        let q = output.get::<Table>("q").unwrap();
        for (index, expected) in [sdk_q.x, sdk_q.y, sdk_q.z, sdk_q.w].into_iter().enumerate() {
            assert!((q.get::<f32>(index + 1).unwrap() - expected).abs() < 1.0e-4);
        }

        let round = output.get::<Table>("round").unwrap();
        let euler = sdk_q.to_euler();
        for (index, expected) in [euler.x, euler.y, euler.z].into_iter().enumerate() {
            assert!((round.get::<f32>(index + 1).unwrap() - expected).abs() < 1.0e-3);
        }

        let sdk_look = SdkQuat::look_rotation(SdkVec3::Y, SdkVec3::Y);
        let look = output.get::<Table>("look").unwrap();
        for (index, expected) in [sdk_look.x, sdk_look.y, sdk_look.z, sdk_look.w]
            .into_iter()
            .enumerate()
        {
            assert!((look.get::<f32>(index + 1).unwrap() - expected).abs() < 1.0e-4);
        }

        let sdk_half = SdkQuat::from_euler(SdkVec3::new(0.0, 180.0, 0.0))
            .slerp(SdkQuat::from_euler(SdkVec3::new(0.0, -180.0, 0.0)), 0.5);
        let half = output.get::<Table>("half").unwrap();
        for (index, expected) in [sdk_half.x, sdk_half.y, sdk_half.z, sdk_half.w]
            .into_iter()
            .enumerate()
        {
            assert!((half.get::<f32>(index + 1).unwrap() - expected).abs() < 1.0e-4);
        }
    }
}
