//! "pblite": protobuf messages as JSON arrays indexed by (field number − 1).
//! Nested messages are nested arrays, bytes are std-base64 strings, enums are ints,
//! and fields tagged `(pblite.pblite_binary)` are binary-proto-then-base64 (or, for
//! strings, base64 of the string). Missing fields are `null`.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use prost::Message;
use prost_reflect::{DynamicMessage, FieldDescriptor, Kind, ReflectMessage, Value};
use serde_json::Value as J;

const PBLITE_BINARY_EXT: u32 = 50000;

fn is_binary(f: &FieldDescriptor) -> bool {
    f.options().extensions().any(|(ed, v)| ed.number() == PBLITE_BINARY_EXT && v.as_bool() == Some(true))
}

pub fn to_json(msg: &DynamicMessage) -> Result<J> {
    let desc = msg.descriptor();
    let max = desc.fields().map(|f| f.number()).max().unwrap_or(0) as usize;
    let mut out = vec![J::Null; max];
    for f in desc.fields() {
        if !msg.has_field(&f) { continue; }
        let v = msg.get_field(&f);
        out[f.number() as usize - 1] = if f.is_list() {
            J::Array(v.as_list().unwrap().iter().map(|x| one_to_json(&f, x)).collect::<Result<_>>()?)
        } else {
            one_to_json(&f, &v)?
        };
    }
    Ok(J::Array(out))
}

fn one_to_json(f: &FieldDescriptor, v: &Value) -> Result<J> {
    Ok(match (f.kind(), v) {
        (Kind::Message(_), Value::Message(m)) => {
            if is_binary(f) { J::String(B64.encode(m.encode_to_vec())) } else { to_json(m)? }
        }
        (_, Value::Bytes(b)) => J::String(B64.encode(b)),
        (_, Value::String(s)) => J::String(if is_binary(f) { B64.encode(s.as_bytes()) } else { s.clone() }),
        (_, Value::I32(i)) => J::from(*i),
        (_, Value::I64(i)) => J::from(*i),
        (_, Value::U32(i)) => J::from(*i),
        (_, Value::U64(i)) => J::from(*i),
        (_, Value::F32(x)) => J::from(*x),
        (_, Value::F64(x)) => J::from(*x),
        (_, Value::Bool(b)) => J::Bool(*b),
        (_, Value::EnumNumber(n)) => J::from(*n),
        (k, v) => bail!("pblite: unsupported {k:?} / {v:?} in {}", f.full_name()),
    })
}

pub fn from_json(j: &J, desc: prost_reflect::MessageDescriptor) -> Result<DynamicMessage> {
    let J::Array(arr) = j else { bail!("pblite: expected array for {}", desc.full_name()) };
    let mut msg = DynamicMessage::new(desc.clone());
    for f in desc.fields() {
        let idx = f.number() as usize - 1;
        let Some(v) = arr.get(idx) else { continue };
        if v.is_null() { continue; }
        let val = if f.is_list() {
            let J::Array(items) = v else { bail!("pblite: expected array for repeated {}", f.full_name()) };
            Value::List(items.iter().map(|x| one_from_json(&f, x)).collect::<Result<_>>()?)
        } else {
            one_from_json(&f, v)?
        };
        msg.set_field(&f, val);
    }
    Ok(msg)
}

fn num(v: &J, f: &FieldDescriptor) -> Result<f64> {
    match v {
        J::Number(n) => n.as_f64().context("bad number"),
        J::String(s) => s.parse::<f64>().with_context(|| format!("pblite: bad numeric string in {}", f.full_name())),
        J::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        _ => bail!("pblite: expected number for {}, got {v}", f.full_name()),
    }
}

fn one_from_json(f: &FieldDescriptor, v: &J) -> Result<Value> {
    Ok(match f.kind() {
        Kind::Message(md) => {
            if is_binary(f) {
                let J::String(s) = v else { bail!("pblite: expected b64 string for {}", f.full_name()) };
                let raw = B64.decode(s)?;
                Value::Message(DynamicMessage::decode(md, raw.as_slice())?)
            } else {
                Value::Message(from_json(v, md)?)
            }
        }
        Kind::Bytes => {
            let J::String(s) = v else { bail!("pblite: expected b64 string for {}", f.full_name()) };
            Value::Bytes(bytes::Bytes::from(B64.decode(s)?))
        }
        Kind::String => {
            let J::String(s) = v else { bail!("pblite: expected string for {}", f.full_name()) };
            Value::String(if is_binary(f) { String::from_utf8(B64.decode(s)?)? } else { s.clone() })
        }
        Kind::Bool => Value::Bool(num(v, f)? != 0.0),
        Kind::Enum(_) => Value::EnumNumber(num(v, f)? as i32),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(num(v, f)? as i32),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(match v { J::String(s) => s.parse()?, _ => num(v, f)? as i64 }),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(num(v, f)? as u32),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(match v { J::String(s) => s.parse()?, _ => num(v, f)? as u64 }),
        Kind::Float => Value::F32(num(v, f)? as f32),
        Kind::Double => Value::F64(num(v, f)?),
    })
}

/// Encode a typed prost message as pblite JSON bytes.
pub fn encode<M: ReflectMessage>(m: &M) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&to_json(&m.transcode_to_dynamic())?)?)
}

/// Decode pblite JSON bytes into a typed prost message.
pub fn decode<M: ReflectMessage + Default>(data: &[u8]) -> Result<M> {
    let j: J = serde_json::from_slice(data).context("pblite: invalid JSON")?;
    decode_value(&j)
}

pub fn decode_value<M: ReflectMessage + Default>(j: &J) -> Result<M> {
    let dm = from_json(j, M::default().descriptor())?;
    Ok(dm.transcode_to::<M>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gm::proto::rpc::*;
    use crate::gm::proto::authentication::*;
    #[test]
    fn roundtrip_and_binary_ext() {
        let m = OutgoingRpcMessage {
            mobile: Some(Device { user_id: 7, source_id: "abc".into(), network: "Bugle".into() }),
            data: Some(outgoing_rpc_message::Data { request_id: "r".into(), bugle_route: BugleRoute::DataEvent as i32, message_data: vec![1, 2, 3], message_type_data: None }),
            auth: None, ttl: 5, dest_registration_i_ds: vec!["hello".into()],
        };
        let j = to_json(&m.transcode_to_dynamic()).unwrap();
        // field 1 mobile → [7,"abc","Bugle"]; field 2 data → ["r",19,...,"AQID" at idx 11]; field 5 ttl; field 9 binary string → base64
        assert_eq!(j[0], serde_json::json!([7, "abc", "Bugle"]));
        assert_eq!(j[1][0], "r"); assert_eq!(j[1][1], 19); assert_eq!(j[1][11], "AQID");
        assert_eq!(j[4], 5);
        assert_eq!(j[8], serde_json::json!(["aGVsbG8="]), "pblite_binary extension must be honoured");
        let back: OutgoingRpcMessage = decode_value(&j).unwrap();
        assert_eq!(back, m);
    }
    #[test]
    fn decode_lenient_numbers() {
        let j = serde_json::json!([null, null, "42"]); // int64 as string
        let d: IncomingRpcMessage = decode_value(&j).unwrap();
        assert_eq!(d.start_execute, 42);
    }
}
