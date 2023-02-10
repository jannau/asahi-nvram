/* SPDX-License-Identifier: MIT */

use std::{
    env,
    fmt::Debug,
    fs,
    fs::OpenOptions,
    io::{self, stdout, Read, Write},
    path::Path,
};

use nvram::{Nvram, Section, UnescapeVal, Variable};

use ini::Ini;

#[derive(Debug)]
enum Error {
    Parse,
    SectionTooBig,
    VariableNotFound,
    UnknownPartition,
    FileIO,
    BluezConfigDirNotFound,
    SliceError,
}

impl From<nvram::Error> for Error {
    fn from(e: nvram::Error) -> Self {
        match e {
            nvram::Error::ParseError => Error::Parse,
            nvram::Error::SectionTooBig => Error::SectionTooBig,
        }
    }
}

impl From<io::Error> for Error {
    fn from(_e: io::Error) -> Self {
        Error::FileIO
    }
}

impl From<std::array::TryFromSliceError> for Error {
    fn from(_e: std::array::TryFromSliceError) -> Self {
        Error::SliceError
    }
}

type Result<T> = std::result::Result<T, Error>;

fn main() {
    real_main().unwrap();
}

fn real_main() -> Result<()> {
    let matches = clap::command!()
        .arg(clap::arg!(-d --device [DEVICE] "Path to the nvram device."))
        .subcommand(clap::Command::new("list").about("Parse shared Bluetooth keys from nvram"))
        .subcommand(
            clap::Command::new("sync")
                .about("Sync Bluetooth device information from nvram")
                .arg(clap::arg!(-c --config [CONFIG] "Bluez config path."))
                .arg(clap::Arg::new("variable").multiple_values(true)),
        )
        .subcommand(clap::Command::new("dump").about("Dump binary Bluetooth device info frm nvram"))
        .get_matches();

    let default_name = "/dev/mtd0ro".to_owned();
    let default_config = "/var/lib/bluetooth".to_owned();
    let bt_var = "BluetoothUHEDevices";

    let mut file = OpenOptions::new()
        .read(true)
        .open(matches.get_one::<String>("device").unwrap_or(&default_name))
        .unwrap();
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    let mut nv = Nvram::parse(&data)?;
    let bt_devs = part_by_name("system", &mut nv)?
        .values
        .get(bt_var.as_bytes())
        .ok_or(Error::VariableNotFound)?;

    match matches.subcommand() {
        Some(("list", _args)) => {
            print_btkeys(bt_devs).expect("Failed to parse bt device info");
        }
        Some(("sync", args)) => {
            sync_btkeys(
                bt_devs,
                args.get_one::<String>("config").unwrap_or(&default_config),
            )
            .expect("Failed to sync bt device info");
        }
        Some(("dump", _args)) => {
            dump(bt_devs).expect("Failed to dump bt device info");
        }
        _ => {
            print_btkeys(bt_devs).expect("Failed to parse bt device info");
        }
    }
    Ok(())
}

fn part_by_name<'a, 'b, 'c>(name: &'a str, nv: &'c mut Nvram<'b>) -> Result<&'c mut Section<'b>> {
    match name {
        "common" => Ok(&mut nv.active_part_mut().common),
        "system" => Ok(&mut nv.active_part_mut().system),
        _ => Err(Error::UnknownPartition),
    }
}

fn dump(var: &Variable) -> Result<()> {
    let mut data = Vec::new();
    for c in UnescapeVal::new(var.value.iter().copied()) {
        data.push(c)
    }

    stdout().write_all(&data)?;
    Ok(())
}

struct BtDevice {
    mac: [u8; 6],
    class: u16,
    name: String,
    vendor_id: u16,
    product_id: u16,
    pairing_key: [u8; 16],
}

struct BtInfo {
    mac: [u8; 6],
    devices: Vec<BtDevice>,
}

fn read_le_u16(input: &mut &[u8]) -> Result<u16> {
    let (int_bytes, rest) = input.split_at(std::mem::size_of::<u16>());
    *input = rest;
    Ok(u16::from_le_bytes(int_bytes.try_into()?))
}

fn parse_bt_device(input: &mut &[u8]) -> Result<BtDevice> {
    // parse MAC
    let (mac_bytes, remain) = input.split_at(6_usize);
    *input = remain;
    let mac: [u8; 6] = mac_bytes.try_into().expect("end of bytes");

    let class = read_le_u16(input)?;

    // skip 2 bytes
    *input = &input[2..];

    // parse device name (u16_le length + \0 terminated utf-8 string)
    let name_len = read_le_u16(input)? as usize;
    let (name_bytes, remain) = input.split_at(name_len);
    *input = remain;
    let name = String::from_utf8_lossy(&name_bytes[..name_len - 1]).to_string();

    // parse pairing key
    let (key_bytes, remain) = input.split_at(16);
    *input = remain;
    let key: [u8; 16] = key_bytes.try_into().expect("end of bytes");

    // parse product / vendor id
    let product_id = read_le_u16(input)?;
    let vendor_id = read_le_u16(input)?;

    // skip 2 unknown trailing bytes
    *input = &input[2..];

    Ok(BtDevice {
        mac,
        class,
        name,
        vendor_id,
        product_id,
        pairing_key: key,
    })
}

fn parse_bt_info(var: &Variable) -> Result<BtInfo> {
    let mut data = Vec::new();
    for c in UnescapeVal::new(var.value.iter().copied()) {
        data.push(c)
    }

    assert!(data.len() >= 8);
    let adapter_mac: [u8; 6] = data[0..6].try_into()?;
    let num_devices = data[6];
    assert!(data[7] == 0x04);

    let mut dev_data = &data[8..];

    let mut devices: Vec<BtDevice> = Vec::new();
    for _n in 0..num_devices {
        devices.push(parse_bt_device(&mut dev_data)?);
    }

    Ok(BtInfo {
        mac: adapter_mac,
        devices,
    })
}

fn format_mac(mac: &[u8; 6]) -> Result<String> {
    Ok(mac
        .iter()
        .map(|x| format!("{:02X}", x))
        .collect::<Vec<String>>()
        .join(":"))
}

fn format_key(key: &[u8; 16]) -> Result<String> {
    Ok(key.iter().map(|x| format!("{:02X}", x)).rev().collect())
}

fn print_btkeys(var: &Variable) -> Result<()> {
    let info = parse_bt_info(var)?;

    for dev in info.devices {
        println!(
            "ID {:04x}:{:04x} {} ({})",
            dev.vendor_id,
            dev.product_id,
            dev.name,
            format_mac(&dev.mac)?
        );
    }
    Ok(())
}

fn sync_btkeys(var: &Variable, config: &String) -> Result<()> {
    let config_path = Path::new(config);

    if !config_path.is_dir() {
        return Err(Error::BluezConfigDirNotFound);
    }

    let info = parse_bt_info(var)?;

    let adapter_path = config_path.join(format_mac(&info.mac)?);

    if !adapter_path.is_dir() {
        fs::create_dir(adapter_path.clone())?;
    }

    for dev in info.devices {
        let dev_path = adapter_path.join(format_mac(&dev.mac)?);

        if !dev_path.is_dir() {
            fs::create_dir(dev_path.clone())?;
        }

        let info_file = dev_path.as_path().join("info");

        if info_file.exists() {
            continue;
        }

        let mut info = Ini::new();

        info.with_section(Some("General"))
            .set("Name", dev.name)
            .set("Class", format!("{:#08X}", dev.class))
            .set("Trusted", "true")
            .set("Blocked", "false")
            .set("WakeAllowed", "true");
        info.with_section(Some("LinkKey"))
            .set("Key", format_key(&dev.pairing_key)?);
        info.with_section(Some("DeviceID"))
            .set("Vendor", format!("{}", dev.vendor_id))
            .set("Product", format!("{}", dev.product_id));
        info.write_to_file(info_file)?;

        println!("{}", format_mac(&dev.mac)?);
    }
    Ok(())
}
