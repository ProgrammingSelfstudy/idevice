//! NSKeyedArchiver 编解码——这是 Apple 公开文档化的二进制归档格式(不是这次
//! 要逆向的私有协议),`$archiver`/`$version`/`$top`/`$objects` 这套结构、
//! `Uid` 对象引用的语义都是标准 Foundation 归档惯例。
//!
//! **解码这边故意跟同类实现(包括最初参考过的 `idevice`)不一样**:那些实现
//! 只对几个白名单里的 Foundation 集合类(`NSString`/`NSData`/`NSDictionary`/
//! `NSArray`/`NSSet`)做"展开"(把 `NS.keys`/`NS.objects` 这种内部表示还原成
//! 普通 plist 结构),遇到不认识的自定义类(比如 Instruments 协议自己的
//! `DTTapStatusMessage` 之类)就原样返回,里面的字段依然是没解析的 `Uid`
//! 引用——真机联调时踩到过这个坑,`sysmontap` 的 tap 消息就是不认识的自定义
//! 类,导致 `Processes`/`System` 这些字段读出来全是解不开的 `Uid(n)`。这里的
//! 解码器**不预设白名单**,对任何字典形状的归档对象,除了 `$class` 这个纯
//! 元数据字段,其余每个字段的值只要是 `Uid` 就统统递归展开——这不是"猜"出来
//! 的,是理解了 `$objects`/`Uid` 引用这套机制本身之后的自然结果:不管是不是
//! Apple 内置类,一个字段的值是不是"需要再查一层 `$objects`",这套引用机制
//! 对所有类都是一样的,没有必要分类讨论。

use plist::{Dictionary, Uid, Value};

#[derive(Debug, thiserror::Error)]
pub enum NkaError {
    #[error("failed to parse archive plist: {0}")]
    PlistError(#[from] plist::Error),
    #[error("archive is missing or has malformed field: {0}")]
    MalformedArchive(&'static str),
}

const ARCHIVER: &str = "NSKeyedArchiver";
const ARCHIVER_VERSION: i64 = 100_000;

/// 把一个普通 plist 值编码成 NSKeyedArchiver 格式的二进制 plist 字节。
pub fn encode(value: Value) -> Vec<u8> {
    let mut objects = vec![Value::String("$null".to_string())];
    let root_uid = encode_object(value, &mut objects);

    let mut top = Dictionary::new();
    top.insert("root".to_string(), Value::Uid(root_uid));

    let mut root = Dictionary::new();
    root.insert("$archiver".to_string(), Value::String(ARCHIVER.to_string()));
    root.insert("$version".to_string(), Value::Integer(ARCHIVER_VERSION.into()));
    root.insert("$top".to_string(), Value::Dictionary(top));
    root.insert("$objects".to_string(), Value::Array(objects));

    let archive = Value::Dictionary(root);
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &archive).expect("encoding a Value to binary plist cannot fail");
    buf
}

fn encode_object(value: Value, objects: &mut Vec<Value>) -> Uid {
    match value {
        Value::String(s) if s == "$null" => Uid::new(0),
        Value::String(s) => push(Value::String(s), objects),
        Value::Integer(_) | Value::Real(_) | Value::Boolean(_) | Value::Date(_) | Value::Data(_) => {
            push(value, objects)
        }
        Value::Dictionary(dict) => encode_dictionary(dict, objects),
        Value::Array(items) => encode_array(items, objects),
        // 不支持的类型(嵌套 Uid 之类不该出现在"要编码的普通值"里)直接编码成
        // null——这个方向(我们自己构造请求参数)不需要处理这些边缘情况。
        _ => Uid::new(0),
    }
}

fn push(value: Value, objects: &mut Vec<Value>) -> Uid {
    objects.push(value);
    Uid::new(objects.len() as u64 - 1)
}

fn encode_dictionary(dict: Dictionary, objects: &mut Vec<Value>) -> Uid {
    let mut keys = Vec::new();
    let mut vals = Vec::new();
    for (k, v) in dict {
        keys.push(Value::Uid(encode_object(Value::String(k), objects)));
        vals.push(Value::Uid(encode_object(v, objects)));
    }
    let class_uid = class_reference("NSDictionary", objects);
    let mut structure = Dictionary::new();
    structure.insert("$class".to_string(), Value::Uid(class_uid));
    structure.insert("NS.keys".to_string(), Value::Array(keys));
    structure.insert("NS.objects".to_string(), Value::Array(vals));
    push(Value::Dictionary(structure), objects)
}

fn encode_array(items: Vec<Value>, objects: &mut Vec<Value>) -> Uid {
    let vals: Vec<Value> = items
        .into_iter()
        .map(|v| Value::Uid(encode_object(v, objects)))
        .collect();
    let class_uid = class_reference("NSArray", objects);
    let mut structure = Dictionary::new();
    structure.insert("$class".to_string(), Value::Uid(class_uid));
    structure.insert("NS.objects".to_string(), Value::Array(vals));
    push(Value::Dictionary(structure), objects)
}

fn class_reference(name: &str, objects: &mut Vec<Value>) -> Uid {
    for (i, obj) in objects.iter().enumerate() {
        if let Some(d) = obj.as_dictionary()
            && d.get("$classname").and_then(|v| v.as_string()) == Some(name)
        {
            return Uid::new(i as u64);
        }
    }
    let mut class_dict = Dictionary::new();
    class_dict.insert(
        "$classes".to_string(),
        Value::Array(vec![Value::String(name.to_string())]),
    );
    class_dict.insert("$classname".to_string(), Value::String(name.to_string()));
    push(Value::Dictionary(class_dict), objects)
}

/// 解出一个 NSKeyedArchiver 归档的二进制/XML plist 字节,还原成不带任何
/// `Uid`/`$class` 元数据痕迹的普通 plist 值。
pub fn decode(bytes: &[u8]) -> Result<Value, NkaError> {
    let archive = Value::from_reader(std::io::Cursor::new(bytes))?;
    let dict = archive
        .into_dictionary()
        .ok_or(NkaError::MalformedArchive("root is not a dictionary"))?;

    let objects = dict
        .get("$objects")
        .and_then(|v| v.as_array())
        .ok_or(NkaError::MalformedArchive("missing $objects"))?
        .clone();

    let root_uid = dict
        .get("$top")
        .and_then(|v| v.as_dictionary())
        .and_then(|top| top.get("root"))
        .and_then(as_uid)
        .ok_or(NkaError::MalformedArchive("missing $top.root"))?;

    decode_object(&objects, root_uid)
}

fn as_uid(v: &Value) -> Option<usize> {
    match v {
        Value::Uid(u) => Some(u.get() as usize),
        _ => None,
    }
}

fn decode_object(objects: &[Value], idx: usize) -> Result<Value, NkaError> {
    let obj = objects
        .get(idx)
        .ok_or(NkaError::MalformedArchive("Uid points past end of $objects"))?;

    match obj {
        Value::String(s) if s == "$null" => Ok(Value::Dictionary(Dictionary::new())),
        Value::String(_) | Value::Integer(_) | Value::Real(_) | Value::Boolean(_) | Value::Date(_) | Value::Data(_) => {
            Ok(obj.clone())
        }
        Value::Uid(u) => decode_object(objects, u.get() as usize),
        Value::Array(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                result.push(match as_uid(item) {
                    Some(i) => decode_object(objects, i)?,
                    None => item.clone(),
                });
            }
            Ok(Value::Array(result))
        }
        Value::Dictionary(d) => decode_archived_object(objects, d),
        other => Ok(other.clone()),
    }
}

/// 一个具体归档对象(`$objects[idx]` 是字典的情况)——先按已知的 Foundation
/// 集合类特殊处理(它们的内部表示 `NS.keys`/`NS.objects`/`NS.string`/
/// `NS.data` 需要按对应结构重新拼起来,不是"直接展开每个字段"能得到正确结果
/// 的),其余情况(包括不认识的自定义类)一律走通用兜底:保留原有的
/// 键,把每个值里的 `Uid` 递归展开,`$class` 这个纯元数据字段丢弃。
fn decode_archived_object(objects: &[Value], d: &Dictionary) -> Result<Value, NkaError> {
    let class_name = d
        .get("$class")
        .and_then(as_uid)
        .and_then(|ci| objects.get(ci))
        .and_then(|v| v.as_dictionary())
        .and_then(|cd| cd.get("$classname"))
        .and_then(|v| v.as_string())
        .unwrap_or("");

    if class_name.contains("String")
        && let Some(s) = d.get("NS.string").and_then(|v| v.as_string())
    {
        return Ok(Value::String(s.to_string()));
    }
    if class_name.contains("Data")
        && let Some(bytes) = d.get("NS.data").and_then(|v| v.as_data())
    {
        return Ok(Value::Data(bytes.to_vec()));
    }
    if class_name.contains("Dictionary") {
        let keys = d.get("NS.keys").and_then(|v| v.as_array());
        let vals = d.get("NS.objects").and_then(|v| v.as_array());
        if let (Some(keys), Some(vals)) = (keys, vals) {
            let mut result = Dictionary::new();
            for (k, v) in keys.iter().zip(vals.iter()) {
                let key_val = match as_uid(k) {
                    Some(i) => decode_object(objects, i)?,
                    None => k.clone(),
                };
                let val_val = match as_uid(v) {
                    Some(i) => decode_object(objects, i)?,
                    None => v.clone(),
                };
                let key_str = match &key_val {
                    Value::String(s) => s.clone(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    other => format!("{other:?}"),
                };
                result.insert(key_str, val_val);
            }
            return Ok(Value::Dictionary(result));
        }
    }
    if (class_name.contains("Array") || class_name.contains("Set"))
        && let Some(items) = d.get("NS.objects").and_then(|v| v.as_array())
    {
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            result.push(match as_uid(item) {
                Some(i) => decode_object(objects, i)?,
                None => item.clone(),
            });
        }
        return Ok(Value::Array(result));
    }

    // 通用兜底:任何其他(包括不认识的)类,当成一个普通字典,把每个字段的
    // `Uid` 值展开,`$class` 丢掉(纯元数据,不是数据本身)。
    let mut result = Dictionary::new();
    for (k, v) in d {
        if k == "$class" {
            continue;
        }
        let decoded = match as_uid(v) {
            Some(i) => decode_object(objects, i)?,
            None => v.clone(),
        };
        result.insert(k.clone(), decoded);
    }
    Ok(Value::Dictionary(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_round_trips() {
        let value = Value::String("hello world".into());
        let encoded = encode(value.clone());
        assert_eq!(decode(&encoded).unwrap(), value);
    }

    #[test]
    fn dictionary_round_trips() {
        let mut dict = Dictionary::new();
        dict.insert("a".into(), Value::Integer(1.into()));
        dict.insert("b".into(), Value::String("s".into()));
        let value = Value::Dictionary(dict);
        assert_eq!(decode(&encode(value.clone())).unwrap(), value);
    }

    #[test]
    fn nested_array_round_trips() {
        let value = Value::Array(vec![
            Value::Integer(1.into()),
            Value::Array(vec![Value::String("nested".into())]),
        ]);
        assert_eq!(decode(&encode(value.clone())).unwrap(), value);
    }

    /// 模拟"不认识的自定义类"场景——手搓一份归档,顶层对象的 `$class` 指向一
    /// 个不在 String/Data/Dictionary/Array/Set 白名单里的类名,验证通用兜底
    /// 逻辑确实会展开它的字段而不是原样返回带 `Uid` 的死数据。这是真机上
    /// `sysmontap` tap 消息(`DTTapStatusMessage` 之类)实际会撞上的场景。
    #[test]
    fn unrecognized_custom_class_fields_are_still_resolved() {
        let mut class_dict = Dictionary::new();
        class_dict.insert("$classname".into(), Value::String("DTTapStatusMessage".into()));

        let mut inner = Dictionary::new();
        inner.insert("Processes".into(), Value::Integer(42.into()));

        let mut top_obj = Dictionary::new();
        top_obj.insert("$class".into(), Value::Uid(Uid::new(1)));
        top_obj.insert("DTTapMessagePlist".into(), Value::Uid(Uid::new(3)));

        let objects = vec![
            Value::String("$null".into()),
            Value::Dictionary(class_dict),
            Value::Dictionary(top_obj),
            Value::Dictionary(inner),
        ];

        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(Uid::new(2)));

        let mut archive = Dictionary::new();
        archive.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
        archive.insert("$version".into(), Value::Integer(100_000.into()));
        archive.insert("$top".into(), Value::Dictionary(top));
        archive.insert("$objects".into(), Value::Array(objects));

        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &Value::Dictionary(archive)).unwrap();

        let decoded = decode(&buf).unwrap();
        let dict = decoded.as_dictionary().unwrap();
        // $class 元数据本身不该出现在结果里。
        assert!(dict.get("$class").is_none());
        let plist_msg = dict.get("DTTapMessagePlist").unwrap().as_dictionary().unwrap();
        assert_eq!(plist_msg.get("Processes").unwrap().as_signed_integer(), Some(42));
    }
}
