/*
 * Copyright 2018-2020 TON DEV SOLUTIONS LTD.
 *
 * Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
 * this file except in compliance with the License.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific TON DEV software governing permissions and
 * limitations under the License.
 */
use crate::config::Config;
use crate::crypto::load_keypair;
use crate::convert;
use ton_abi::{Contract, ParamType};
use chrono::{TimeZone, Local};
use hex;
use std::time::SystemTime;
use ton_client_rs::{
    TonClient, TonClientConfig, TonAddress, EncodedMessage
};
use ton_types::cells_serialization::{BagOfCells};

fn now() -> u32 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as u32
}

fn create_client(conf: &Config) -> Result<TonClient, String> {
    TonClient::new(&TonClientConfig{
        base_url: Some(conf.url.clone()),
        message_retries_count: Some(conf.retries),
        message_expiration_timeout: Some(conf.timeout),
        message_expiration_timeout_grow_factor: Some(1.5),
        message_processing_timeout: Some(conf.timeout),
        message_processing_timeout_grow_factor: Some(1.5),
        wait_for_timeout: None,
        access_key: None,
    })
    .map_err(|e| format!("failed to create tonclient: {}", e.to_string()))
}

pub fn create_client_verbose(conf: &Config) -> Result<TonClient, String> {
    println!("Connecting to {}", conf.url);
    create_client(conf)
}

fn prepare_message(
    ton: &TonClient,
    addr: &TonAddress,
    abi: &str,
    method: &str,
    params: &str,
    header: Option<String>,
    keys: Option<String>,
) -> Result<EncodedMessage, String> {    
    
    let keys = keys.map(|k| load_keypair(&k)).transpose()?;

    ton.contracts.create_run_message(
        addr,
        abi.into(),
        method,
        header.map(|v| v.into()),
        params.into(),
        keys.as_ref(),
        None,
    )
    .map_err(|e| format!("failed to create inbound message: {}", e))
}

fn print_encoded_message(msg: &EncodedMessage) {
    println!();
    println!("MessageId: {}", msg.message_id);
    print!("Expire at: ");
    if msg.expire.is_some() {
        let expire_at = Local.timestamp(msg.expire.unwrap() as i64 , 0);
        println!("{}", expire_at.to_rfc2822());
    } else {
        println!("unknown");
    }
}

fn pack_message(msg: &EncodedMessage, method: &str) -> String {
    let json_msg = json!({
        "msg": {
            "message_id": msg.message_id,
            "message_body": hex::encode(&msg.message_body),
            "expire": msg.expire
        },
        "method": method,
    });

    hex::encode(serde_json::to_string(&json_msg).unwrap())
}

fn unpack_message(str_msg: &str) -> Result<(EncodedMessage, String), String> {
    let bytes = hex::decode(str_msg)
        .map_err(|e| format!("couldn't unpack message: {}", e))?;
    
        let str_msg = std::str::from_utf8(&bytes)
        .map_err(|e| format!("message is corrupted: {}", e))?;

    let json_msg: serde_json::Value = serde_json::from_str(str_msg)
        .map_err(|e| format!("couldn't decode message: {}", e))?;

    let method = json_msg["method"].as_str()
        .ok_or(r#"couldn't find "method" key in message"#)?
        .to_owned();
    let message_id = json_msg["msg"]["message_id"].as_str()
        .ok_or(r#"couldn't find "message_id" key in message"#)?
        .to_owned();
    let message_body = json_msg["msg"]["message_body"].as_str()
        .ok_or(r#"couldn't find "message_body" key in message"#)?;
    let message_body = hex::decode(message_body).unwrap();
    let expire = json_msg["msg"]["expire"].as_u64().map(|x| x as u32);
    
    let msg = EncodedMessage {
        message_id, message_body, expire
    };
    Ok((msg, method))
}

fn decode_call_parameters(ton: &TonClient, msg: &EncodedMessage, abi: &str) -> Result<(String, String), String> {
    let tvm_msg = ton_sdk::Contract::deserialize_message(&msg.message_body[..]).unwrap();
    let body_slice = tvm_msg.body().unwrap();

    let mut data = Vec::new();
    let bag = BagOfCells::with_root(&body_slice.cell());
    bag.write_to(&mut data, false)
        .map_err(|e| format!("couldn't create body BOC: {}", e))?;
        
    let result = ton.contracts.decode_input_message_body(
        abi.into(),
        &data[..],
        false
    ).map_err(|e| format!("couldn't decode message body: {}", e))?;

    Ok((
        result.function,
        serde_json::to_string_pretty(&result.output).unwrap()
    ))
}

fn parse_integer_param(value: &str) -> Result<String, String> {
    let value = value.trim_matches('\"');

    if value.ends_with('T') {
        convert::convert_token(value.trim_end_matches('T'))
    } else {
        Ok(value.to_owned())
    }
}

fn build_json_from_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    let abi_obj = Contract::load(abi.as_bytes()).map_err(|e| format!("failed to parse ABI: {}", e))?;
    let functions = abi_obj.functions();
        
    let func_obj = functions.get(method).unwrap();
    let inputs = func_obj.input_params();

    let mut params_json = json!({ });
    for input in inputs {
        let mut iter = params_vec.iter();
        let _param = iter.find(|x| x.trim_start_matches('-') == input.name)
            .ok_or(format!(r#"argument "{}" of type "{}" not found"#, input.name, input.kind))?;

        let value = iter.next()
            .ok_or(format!(r#"argument "{}" of type "{}" has no value"#, input.name, input.kind))?
            .to_string();

        let value = match input.kind {
            ParamType::Uint(_) | ParamType::Int(_) => {
                json!(parse_integer_param(&value)?)
            },
            ParamType::Array(ref x) => {
                if let ParamType::Uint(_) = **x {
                    let mut result_vec: Vec<String> = vec![];
                    for i in value.split(|c| c == ',' || c == '[' || c == ']') {
                        if i != "" {
                            result_vec.push(parse_integer_param(i)?)
                        }
                    }
                    json!(result_vec)
                } else {
                    json!(value)
                }
            },
            _ => {
                json!(value)
            }
        };
        params_json[input.name.clone()] = value;
    }

    serde_json::to_string(&params_json).map_err(|e| format!("{}", e))
}

pub fn call_contract_with_result(
    conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    local: bool,
) -> Result<serde_json::Value, String> {
    let ton = create_client_verbose(&conf)?;

    let ton_addr = TonAddress::from_str(addr)
        .map_err(|e| format!("failed to parse address: {}", e.to_string()))?;

    let result = if local {
        println!("Running get-method...");
        ton.contracts.run_local(
            &ton_addr,
            None,
            abi.into(),
            method,
            None,
            params.into(),
            None,
            None,
            false
        )
        .map_err(|e| format!("run failed: {}", e.to_string()))?
        .output
        
    } else {
        println!("Generating external inbound message...");
        let msg = prepare_message(
            &ton,
            &ton_addr,
            &abi,
            method,
            params,
            None,
            keys,
        )?;

        print_encoded_message(&msg);
        println!("Processing... ");

        ton.contracts.process_message(msg, Some(abi.into()), Some(method), None)
            .map_err(|e| format!("Failed: {}", e.to_string()))?
            .output
    };
    Ok(result)
}

pub fn call_contract(
    conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    local: bool
) -> Result<(), String> {
    let result = call_contract_with_result(conf, addr, abi, method, params, keys, local)?;

    println!("Succeeded.");
    if !result.is_null() {
        println!("Result: {}", serde_json::to_string_pretty(&result).unwrap());
    }
    Ok(())
}

pub fn generate_message(
    _conf: Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    lifetime: u32,
) -> Result<(), String> {
    let ton = TonClient::default()
        .map_err(|e| format!("failed to create tonclient: {}", e.to_string()))?;

    let ton_addr = TonAddress::from_str(addr)
        .map_err(|e| format!("failed to parse address: {}", e.to_string()))?;

    let expire_at = lifetime + now();
    let header = json!({
        "expire": expire_at
    });

    let msg = prepare_message(
        &ton,
        &ton_addr,
        &abi,
        method,
        params,
        Some(serde_json::to_string(&header).unwrap()),
        keys,
    )?;
    print_encoded_message(&msg);

    let str_msg = pack_message(&msg, method);
    println!("Message: {}", &str_msg);
    println!();
    qr2term::print_qr(&str_msg).unwrap();
    println!();
    Ok(())
}

pub fn call_contract_with_msg(conf: Config, str_msg: String, abi: String) -> Result<(), String> {
    let ton = create_client_verbose(&conf)?;

    let (msg, method) = unpack_message(&str_msg)?;
    print_encoded_message(&msg);

    let params = decode_call_parameters(&ton, &msg, &abi)?;

    println!("Calling method {} with parameters:", params.0);
    println!("{}", params.1);
    println!("Processing... ");
    let result = ton.contracts.process_message(
        msg,
        Some(abi.into()),
        Some(&method),
        None
    )
    .map_err(|e| format!("Failed: {}", e.to_string()))?;

    println!("Succeded.");
    if !result.output.is_null() {
        println!("Result: {}", serde_json::to_string_pretty(&result.output).unwrap());
    }
    Ok(())
}

pub fn parse_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    if params_vec.len() == 1 {
        // if there is only 1 parameter it must be a json string with arguments
        Ok(params_vec[0].to_owned())
    } else {
        build_json_from_params(params_vec, abi, method)
    }
}

pub fn run_get_method(conf: Config, addr: &str, method: &str, params: Option<String>) -> Result<(), String> {
    let ton = create_client_verbose(&conf)?;

    let ton_addr = TonAddress::from_str(addr)
        .map_err(|e| format!("failed to parse address: {}", e.to_string()))?;

    println!("Running get-method...");
    let result = ton.contracts.run_get(
            Some(&ton_addr),
            None,
            method,
            params.map(|p| p.into()),
        )
        .map_err(|e| format!("run failed: {}", e.to_string()))?
        .output;
    
    println!("Succeded.");
    println!("Result: {}", result);
    Ok(())
}