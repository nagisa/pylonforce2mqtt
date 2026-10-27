//! What the registers of the battery mean: pure functions from the register [`Snapshot`] to the
//! text of a property (see `REGISTERS.md`). Nothing here panics on an unexpected word. A reading is
//! `None` if a register it needs has not been read yet and `Some(Err(_))` if it is not usable.

use crate::modbus::{PollBlock, Snapshot, Space};
use std::collections::BTreeSet;
use std::time::Duration;

/// The most modules and cells that a stack can have.
pub(crate) const MAX_MODULES: u16 = 6;
pub(crate) const MAX_CELLS: u16 = 150;

/// How often the registers that never change (for as long as the battery is the same) are read.
const CONSTANT_PERIOD: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy)]
struct Block {
    start: u16,
    count: u16,
}

const IDENTIFICATION: Block = Block { start: 0x1000, count: 12 };
const SERIAL_NUMBER: Block = Block { start: 0x1450, count: 16 };
/// The state word to the last of the flags in one go, so that values made of several words agree.
const STATUS: Block = Block { start: 0x1400, count: 0x1439 - 0x1400 + 1 };
const MODULE_VOLTAGES: Block = Block { start: 0x1460, count: MAX_MODULES };
const MODULE_TEMPERATURES: Block = Block { start: 0x14B0, count: MAX_MODULES };
const CELL_VOLTAGES: Block = Block { start: 0x1500, count: MAX_CELLS };

pub(crate) mod register {
    pub const STATE: u16 = 0x1400;
    pub const PROTECTIONS: u16 = 0x1401;
    pub const ALARMS: u16 = 0x1402;
    pub const VOLTAGE: u16 = 0x1403;
    pub const CURRENT: u16 = 0x1404;
    pub const BMS_TEMPERATURE: u16 = 0x1406;
    pub const STATE_OF_CHARGE: u16 = 0x1407;
    pub const CYCLES: u16 = 0x1408;
    pub const CHARGE_VOLTAGE: u16 = 0x1409;
    pub const CHARGE_CURRENT: u16 = 0x140A;
    pub const DISCHARGE_VOLTAGE: u16 = 0x140C;
    pub const DISCHARGE_CURRENT: u16 = 0x140D;
    pub const MAX_CELL_VOLTAGE: u16 = 0x1410;
    pub const MIN_CELL_VOLTAGE: u16 = 0x1411;
    pub const MAX_CELL_INDEX: u16 = 0x1412;
    pub const MIN_CELL_INDEX: u16 = 0x1413;
    pub const MAX_MODULE_TEMPERATURE: u16 = 0x141C;
    pub const MIN_MODULE_TEMPERATURE: u16 = 0x141D;
    pub const MAX_TEMPERATURE_MODULE_INDEX: u16 = 0x141E;
    pub const MIN_TEMPERATURE_MODULE_INDEX: u16 = 0x141F;
    pub const STATE_OF_HEALTH: u16 = 0x1420;
    pub const REMAINING_ENERGY: u16 = 0x1421;
    pub const CHARGED_TODAY: u16 = 0x1427;
    pub const DISCHARGED_TODAY: u16 = 0x1429;
    pub const CHARGED_TOTAL: u16 = 0x142B;
    pub const DISCHARGED_TOTAL: u16 = 0x142D;
    pub const FORCE_CHARGE_REQUEST: u16 = 0x142F;
    pub const BALANCE_CHARGE_REQUEST: u16 = 0x1430;
    pub const FAULTS: u16 = 0x1433;
    pub const MODULES: u16 = 0x1436;
    pub const CELLS: u16 = 0x1437;
    pub const CHARGE_NOT_ALLOWED: u16 = 0x1438;
    pub const DISCHARGE_NOT_ALLOWED: u16 = 0x1439;
    pub const MANUFACTURER: u16 = 0x1000;
    pub const MODEL: u16 = 0x1005;
    pub const FIRMWARE_VERSION: u16 = 0x100A;
    pub const FIRMWARE_BUILD: u16 = 0x100B;
}
use register as reg;

/// What the battery has said that is not a usable value, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Invalid(pub &'static str);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The text to publish for a property.
pub(crate) type Reading = Option<Result<String, Invalid>>;

/// The registers to poll in order to provide the properties.
///
/// The temperatures of the cells never are: they are the temperature of the module, repeated for
/// every cell in it.
pub(crate) fn poll_plan(period: Duration, detailed: bool) -> Vec<PollBlock> {
    let constant = [IDENTIFICATION, SERIAL_NUMBER].map(|block| (block, CONSTANT_PERIOD));
    // A register more than there is anything at (it reads 0), only for the read to be of another
    // length than that of the module voltages: the length is all that tells the responses apart.
    let temperatures = Block { count: MODULE_TEMPERATURES.count + 1, ..MODULE_TEMPERATURES };
    let changing = [STATUS, MODULE_VOLTAGES, temperatures].map(|block| (block, period));
    let cells = detailed.then_some((CELL_VOLTAGES, period));
    let blocks = constant.into_iter().chain(changing).chain(cells);
    let poll = |(Block { start, count }, period)| PollBlock {
        space: Space::Holding,
        start,
        count,
        period,
    };
    blocks.map(poll).collect()
}

/// Whether everything that is needed to describe the battery has been read at least once.
pub(crate) fn is_complete(snapshot: &Snapshot, detailed: bool) -> bool {
    let needed = [IDENTIFICATION, SERIAL_NUMBER, STATUS, MODULE_VOLTAGES, MODULE_TEMPERATURES];
    let cells = detailed.then_some(CELL_VOLTAGES);
    let read = |Block { start, count }| snapshot.words(Space::Holding, start, count).is_some();
    needed.into_iter().chain(cells).all(read)
}

/// The number of modules and of cells in all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Topology {
    pub modules: u16,
    cells: u16,
}

impl Topology {
    pub fn new(modules: u16, cells: u16) -> Result<Self, Invalid> {
        if !(1..=MAX_MODULES).contains(&modules) {
            return Err(Invalid("the number of modules is out of range"));
        }
        if !(1..=MAX_CELLS).contains(&cells) {
            return Err(Invalid("the number of cells is out of range"));
        }
        if !cells.is_multiple_of(modules) {
            return Err(Invalid("the cells do not divide evenly between the modules"));
        }
        Ok(Topology { modules, cells })
    }

    pub fn read(snapshot: &Snapshot) -> Option<Result<Self, Invalid>> {
        Some(Self::new(word(snapshot, reg::MODULES)?, word(snapshot, reg::CELLS)?))
    }

    pub fn cells_per_module(&self) -> u16 {
        self.cells.checked_div(self.modules).unwrap_or(1)
    }
}

fn word(snapshot: &Snapshot, address: u16) -> Option<u16> {
    snapshot.get(Space::Holding, address)
}

/// A 32-bit number: the high word is at the address and the low word follows.
fn double(snapshot: &Snapshot, address: u16) -> Option<u32> {
    let high = word(snapshot, address)?;
    let low = word(snapshot, address.checked_add(1)?)?;
    Some(u32::from(high) << 16 | u32::from(low))
}

/// The text of a number that is a count of `10^-decimals`: 1234 with 2 decimals is `12.34`.
pub(crate) fn fixed(value: i64, decimals: u32) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    if decimals == 0 {
        return format!("{sign}{magnitude}");
    }
    let scale = 10u64.saturating_pow(decimals);
    let (whole, fraction) = (magnitude / scale, magnitude % scale);
    format!("{sign}{whole}.{fraction:0width$}", width = decimals as usize)
}

/// Division that rounds to the nearest whole number, half away from zero.
fn divide_rounding(numerator: i64, denominator: i64) -> Option<i64> {
    let half = denominator.checked_abs()?.checked_div(2)?;
    let adjusted = numerator.checked_add(if numerator < 0 { -half } else { half })?;
    adjusted.checked_div(denominator)
}

/// How a number is stored in the registers.
#[derive(Clone, Copy)]
pub(crate) enum Num {
    U16,
    I16,
    U32,
    I32,
    /// The discharge current limit is stored negative.
    AbsI32,
}

/// A number from the registers at the address, with `decimals` decimals.
pub(crate) fn number(snapshot: &Snapshot, address: u16, kind: Num, decimals: u32) -> Reading {
    let value = match kind {
        Num::U16 => i64::from(word(snapshot, address)?),
        Num::I16 => i64::from(word(snapshot, address)? as i16),
        Num::U32 => i64::from(double(snapshot, address)?),
        Num::I32 => i64::from(double(snapshot, address)? as i32),
        Num::AbsI32 => i64::from(double(snapshot, address)? as i32).abs(),
    };
    Some(Ok(fixed(value, decimals)))
}

pub(crate) fn percentage(snapshot: &Snapshot, address: u16) -> Reading {
    let percent = word(snapshot, address)?;
    Some(if percent > 100 { Err(Invalid("more than 100 %")) } else { Ok(percent.to_string()) })
}

/// A flag that is 0 or 1; `inverted` is for those that say the opposite of the property.
pub(crate) fn flag(snapshot: &Snapshot, address: u16, inverted: bool) -> Reading {
    Some(match word(snapshot, address)? {
        raw @ (0 | 1) => Ok(((raw == 1) != inverted).to_string()),
        _ => Err(Invalid("neither 0 nor 1")),
    })
}

pub(crate) fn state(snapshot: &Snapshot) -> Reading {
    // The bits 10 to 13: idle, charging, discharging and sleep.
    let state = match (word(snapshot, reg::STATE)? >> 10) & 0xF {
        0b0001 => "idle",
        0b0010 => "charging",
        0b0100 => "discharging",
        0b1000 => "sleep",
        _ => return Some(Err(Invalid("not exactly one of the state bits is set"))),
    };
    Some(Ok(state.to_string()))
}

/// The current in units of 10 mA.
fn current_raw(snapshot: &Snapshot) -> Option<Result<i32, Invalid>> {
    let current = double(snapshot, reg::CURRENT)? as i32;
    let beyond_any_stack = current.unsigned_abs() > 1000 * 100;
    Some(if beyond_any_stack { Err(Invalid("more than 1000 A")) } else { Ok(current) })
}

pub(crate) fn current(snapshot: &Snapshot) -> Reading {
    Some(current_raw(snapshot)?.map(|current| fixed(current.into(), 2)))
}

pub(crate) fn power(snapshot: &Snapshot) -> Reading {
    let voltage = i64::from(word(snapshot, reg::VOLTAGE)?);
    Some(current_raw(snapshot)?.and_then(|current| {
        // 0.1 V times 0.01 A is 0.001 W.
        let milliwatts = voltage.checked_mul(current.into());
        let watts = milliwatts.and_then(|milliwatts| divide_rounding(milliwatts, 1000));
        watts.map(|watts| fixed(watts, 0)).ok_or(Invalid("the power is out of range"))
    }))
}

/// The module that a 0-based index in a register refers to, numbered from 1.
pub(crate) fn module_number(snapshot: &Snapshot, topology: &Topology, address: u16) -> Reading {
    let index = word(snapshot, address)?;
    Some(match index < topology.modules {
        true => Ok((u32::from(index) + 1).to_string()),
        false => Err(Invalid("there is no such module")),
    })
}

fn cell_voltage(snapshot: &Snapshot, address: u16) -> Option<Result<u16, Invalid>> {
    let millivolts = word(snapshot, address)?;
    Some(if millivolts == 0 { Err(Invalid("0 V")) } else { Ok(millivolts) })
}

/// The voltage of the highest or the lowest cell, at the address.
pub(crate) fn extreme_cell_voltage(snapshot: &Snapshot, address: u16) -> Reading {
    Some(cell_voltage(snapshot, address)?.map(|millivolts| fixed(millivolts.into(), 3)))
}

pub(crate) fn cell_voltage_spread(snapshot: &Snapshot) -> Reading {
    let max = cell_voltage(snapshot, reg::MAX_CELL_VOLTAGE)?;
    let min = cell_voltage(snapshot, reg::MIN_CELL_VOLTAGE)?;
    Some(match (max, min) {
        (Ok(max), Ok(min)) if max >= min => Ok(fixed(i64::from(max) - i64::from(min), 3)),
        (Ok(_), Ok(_)) => Err(Invalid("the highest voltage is below the lowest")),
        (Err(invalid), _) | (_, Err(invalid)) => Err(invalid),
    })
}

/// The average voltage of a cell in 0.1 mV, out of the voltage in 10 mV of `cells` of them.
fn average_voltage(centivolts: i64, cells: u16) -> Result<String, Invalid> {
    let average = centivolts.checked_mul(100).and_then(|v| divide_rounding(v, cells.into()));
    average.map(|average| fixed(average, 4)).ok_or(Invalid("the average is out of range"))
}

pub(crate) fn average_cell_voltage(snapshot: &Snapshot, topology: &Topology) -> Reading {
    let mut total = 0i64;
    for module in 0..topology.modules {
        let voltage = word(snapshot, MODULE_VOLTAGES.start.checked_add(module)?)?;
        if voltage == 0 {
            return Some(Err(Invalid("a module is at 0 V")));
        }
        total += i64::from(voltage);
    }
    Some(average_voltage(total, topology.cells))
}

/// The module and the cell in it (both from 1) of the cell that a 0-based index in a register
/// refers to; `pick` chooses which of the two is the reading.
pub(crate) fn cell_location(
    snapshot: &Snapshot,
    topology: &Topology,
    address: u16,
    pick: fn(u16, u16) -> u16,
) -> Reading {
    let index = word(snapshot, address)?;
    let per_module = topology.cells_per_module();
    Some(match index < topology.cells {
        true => Ok(pick(index / per_module + 1, index % per_module + 1).to_string()),
        false => Err(Invalid("there is no such cell")),
    })
}

/// A flag bit is a condition. Each bit that is set is named; the unknown ones are named after the
/// number of the bit.
pub(crate) type BitNames = &'static [(u16, &'static str)];

/// Some of the conditions have two bits.
pub(crate) const PROTECTION_BITS: BitNames = &[
    (15, "module-shutdown-voltage"),
    (14, "cell-over-voltage"),
    (13, "cell-under-voltage"),
    (12, "over-temperature"),
    (10, "short-circuit"),
    (9, "discharge-over-current"),
    (8, "charge-over-current"),
    (7, "module-over-temperature"),
    (6, "module-under-temperature"),
    (5, "module-over-temperature"),
    (4, "module-under-temperature"),
    (3, "stack-over-voltage"),
    (2, "stack-under-voltage"),
    (1, "module-over-voltage"),
    (0, "module-under-voltage"),
];

pub(crate) const ALARM_BITS: BitNames = &[
    (14, "cell-high-voltage"),
    (13, "cell-low-voltage"),
    (12, "cell-high-temperature"),
    (11, "stack-high-temperature"),
    (9, "discharge-over-current-alarm"),
    (8, "charge-over-current-alarm"),
    (7, "module-high-temperature"),
    (6, "module-low-temperature"),
    (5, "module-high-temperature"),
    (4, "module-low-temperature"),
    (3, "stack-high-voltage"),
    (2, "stack-low-voltage"),
    (1, "module-high-voltage"),
    (0, "module-low-voltage"),
];

pub(crate) const FAULT_BITS: BitNames = &[
    (10, "initialisation-mismatch"),
    (9, "i2c-fault"),
    (8, "bmic-fault"),
    (7, "short-circuit"),
    (6, "battery-damaged"),
    (5, "relay-fault"),
    (4, "reverse-voltage"),
    (3, "dc-over-voltage"),
    (2, "module-offline"),
    (1, "temperature-sensing-fault"),
    (0, "voltage-sensing-fault"),
];

/// The conditions that the set bits stand for, as a sorted JSON array without repeats.
pub(crate) fn conditions(snapshot: &Snapshot, address: u16, names: BitNames) -> Reading {
    let bits = word(snapshot, address)?;
    let name = |bit| match names.iter().find(|(named, _)| *named == bit) {
        Some((_, name)) => name.to_string(),
        None => format!("bit-{bit}"),
    };
    let conditions: BTreeSet<String> =
        (0..u16::BITS as u16).filter(|bit| bits & (1 << bit) != 0).map(name).collect();
    Some(serde_json::to_string(&conditions).map_err(|_| Invalid("cannot be made into JSON")))
}

/// The address of the word for the module (counted from 0) in the array that starts at `start`.
fn module_address(start: u16, topology: &Topology, module: u16) -> Option<u16> {
    (module < topology.modules).then(|| start.checked_add(module)).flatten()
}

fn module_centivolts(
    snapshot: &Snapshot,
    topology: &Topology,
    module: u16,
) -> Option<Result<u16, Invalid>> {
    let centivolts = word(snapshot, module_address(MODULE_VOLTAGES.start, topology, module)?)?;
    Some(if centivolts == 0 { Err(Invalid("0 V")) } else { Ok(centivolts) })
}

pub(crate) fn module_voltage(snapshot: &Snapshot, topology: &Topology, module: u16) -> Reading {
    Some(module_centivolts(snapshot, topology, module)?.map(|v| fixed(v.into(), 2)))
}

pub(crate) fn module_average_cell_voltage(
    snapshot: &Snapshot,
    topology: &Topology,
    module: u16,
) -> Reading {
    let centivolts = module_centivolts(snapshot, topology, module)?;
    Some(centivolts.and_then(|v| average_voltage(v.into(), topology.cells_per_module())))
}

pub(crate) fn module_temperature(snapshot: &Snapshot, topology: &Topology, module: u16) -> Reading {
    let address = module_address(MODULE_TEMPERATURES.start, topology, module)?;
    number(snapshot, address, Num::I16, 1)
}

/// The highest and the lowest voltage among the cells of a module in mV, each with the cell
/// (from 1) that has it: the first of them if more than one do.
pub(crate) struct CellExtremes {
    pub max: (u16, u16),
    pub min: (u16, u16),
}

/// The text for the extremes of the cells of the module (counted from 0). There are none to speak
/// of if a cell is not known to have a voltage.
pub(crate) fn module_cells(
    snapshot: &Snapshot,
    topology: &Topology,
    module: u16,
    text: fn(&CellExtremes) -> String,
) -> Reading {
    if module >= topology.modules {
        return None;
    }
    let cells = (0..topology.cells_per_module()).map(|cell| {
        let voltage = single_cell_voltage_mv(snapshot, topology, module, cell);
        Ok((voltage.unwrap_or(Err(Invalid("a cell has not been read")))?, cell + 1))
    });
    let cells: Result<Vec<_>, Invalid> = cells.collect();
    Some(cells.and_then(|cells| {
        let max = cells.iter().copied().reduce(|a, b| if b.0 > a.0 { b } else { a });
        let min = cells.iter().copied().reduce(|a, b| if b.0 < a.0 { b } else { a });
        let (max, min) = max.zip(min).ok_or(Invalid("the module has no cells"))?;
        Ok(text(&CellExtremes { max, min }))
    }))
}

/// The voltage of a cell in mV, the cell and the module counted from 0.
fn single_cell_voltage_mv(
    snapshot: &Snapshot,
    topology: &Topology,
    module: u16,
    cell: u16,
) -> Option<Result<u16, Invalid>> {
    let per_module = topology.cells_per_module();
    if module >= topology.modules || cell >= per_module {
        return None;
    }
    let index = module.checked_mul(per_module)?.checked_add(cell)?;
    cell_voltage(snapshot, CELL_VOLTAGES.start.checked_add(index)?)
}

/// The voltage of a cell, the cell and the module counted from 0.
pub(crate) fn single_cell_voltage(
    snapshot: &Snapshot,
    topology: &Topology,
    module: u16,
    cell: u16,
) -> Reading {
    let voltage = single_cell_voltage_mv(snapshot, topology, module, cell)?;
    Some(voltage.map(|millivolts| fixed(millivolts.into(), 3)))
}

pub(crate) fn firmware_version(snapshot: &Snapshot) -> Reading {
    let version = word(snapshot, reg::FIRMWARE_VERSION)?.to_be_bytes();
    let build = word(snapshot, reg::FIRMWARE_BUILD)?.to_be_bytes();
    // The build is `B{b2}.{b1}.{b0}`, but there is no word for the last.
    Some(Ok(format!("{}.{} (B{}.{})", version[0], version[1], build[0], build[1])))
}

/// The text in the `count` registers from the `start`, two characters in each, the first of them
/// in the high byte, without the padding at the end. An error if what is left is empty or is not
/// printable.
fn text(snapshot: &Snapshot, start: u16, count: u16) -> Reading {
    let mut bytes = Vec::with_capacity(usize::from(count) * 2);
    for i in 0..count {
        bytes.extend(word(snapshot, start.checked_add(i)?)?.to_be_bytes());
    }
    let length = bytes.iter().rposition(|&b| b != 0 && b != b' ').map_or(0, |last| last + 1);
    bytes.truncate(length);
    Some(if bytes.is_empty() {
        Err(Invalid("empty"))
    } else if !bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        Err(Invalid("not printable ASCII"))
    } else {
        Ok(bytes.into_iter().map(char::from).collect())
    })
}

pub(crate) fn serial_number(snapshot: &Snapshot) -> Reading {
    text(snapshot, SERIAL_NUMBER.start, SERIAL_NUMBER.count)
}

/// What the battery calls itself: the manufacturer and the model, if it says so properly.
pub(crate) fn name(snapshot: &Snapshot) -> Option<String> {
    let manufacturer = text(snapshot, reg::MANUFACTURER, 5)?.ok()?;
    let model = text(snapshot, reg::MODEL, 5)?.ok()?;
    Some(format!("{manufacturer} {model}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Plausible registers of a battery with four modules of 30 cells, discharging at 0.11 A.
    pub(crate) fn fixture() -> Snapshot {
        let mut snapshot = Snapshot::new();
        let mut set = |address: u16, words: &[u16]| {
            for (i, &word) in words.iter().enumerate() {
                snapshot.set(Space::Holding, address + i as u16, word);
            }
        };
        // Pylon, Force_H2, V2.0, B52.34.
        set(0x1000, &[0x5079, 0x6c6f, 0x6e00, 0, 0]);
        set(0x1005, &[0x466f, 0x7263, 0x655f, 0x4832, 0]);
        set(0x1009, &[0]);
        set(0x100A, &[0x0200, 0x3422]);
        let mut status = [0u16; 58];
        let mut put = |offset: usize, words: &[u16]| {
            status[offset..offset + words.len()].copy_from_slice(words);
        };
        put(0x00, &[0x1002, 0, 0, 0x0EF9, 0xFFFF, 0xFFF5, 320, 18, 123]);
        put(0x09, &[4320, 0, 1850, 3480, 0xFFFF, 0xF8C6, 0x0003]);
        put(0x10, &[3202, 3188, 56, 47, 200, 190, 90, 0, 9589, 9579, 1, 3, 200, 190, 3, 0]);
        put(0x20, &[98, 0, 3100]);
        put(0x27, &[0, 1234, 0, 5678, 0, 2100, 0, 1900]);
        put(0x36, &[4, 120, 0, 0]);
        set(0x1400, &status);
        let serial: Vec<u16> =
            b"PFH2TEST00000001".chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        set(0x1450, &serial);
        set(0x1458, &[0; 8]);
        set(0x1460, &[9583, 9589, 9581, 9579, 0, 0]);
        set(0x14B0, &[190, 190, 190, 200, 0, 0]);
        let cells: Vec<u16> = (0..150).map(|i| if i < 120 { 3190 + i % 5 } else { 0 }).collect();
        set(0x1500, &cells);
        snapshot
    }

    pub(crate) fn topology() -> Topology {
        Topology::new(4, 120).unwrap()
    }

    fn ok(reading: Reading) -> String {
        reading.expect("has been read").expect("is valid")
    }

    fn invalid(reading: Reading) -> &'static str {
        reading.expect("has been read").expect_err("is not valid").0
    }

    fn set(s: &mut Snapshot, address: u16, word: u16) {
        s.set(Space::Holding, address, word);
    }

    #[test]
    fn numbers_are_written_with_all_of_their_decimals() {
        for (value, decimals, text) in [
            (3833, 1, "383.3"),
            (-11, 2, "-0.11"),
            (0, 2, "0.00"),
            (5, 3, "0.005"),
            (-5, 1, "-0.5"),
            (-12345, 0, "-12345"),
            (i64::MIN, 4, "-922337203685477.5808"),
        ] {
            assert_eq!(fixed(value, decimals), text);
        }
        for (numerator, denominator, quotient) in
            [(1, 2, Some(1)), (-1, 2, Some(-1)), (-42_163, 1000, Some(-42)), (7, 0, None)]
        {
            assert_eq!(divide_rounding(numerator, denominator), quotient);
        }
        assert_eq!(divide_rounding(i64::MAX, 2), None);
    }

    #[test]
    fn nothing_is_made_up_for_registers_that_have_not_been_read() {
        let (s, t) = (Snapshot::new(), topology());
        assert_eq!(Topology::read(&s), None);
        for reading in [state(&s), current(&s), power(&s), serial_number(&s), firmware_version(&s)]
        {
            assert_eq!(reading, None);
        }
        assert_eq!(average_cell_voltage(&s, &t), None);
        assert_eq!(module_voltage(&s, &t, 0), None);
        assert_eq!(single_cell_voltage(&s, &t, 0, 0), None);
        assert_eq!(conditions(&s, reg::FAULTS, FAULT_BITS), None);
        assert_eq!(name(&s), None);
        assert!(!is_complete(&s, false));
        // Half of a 32-bit number is not a number.
        let mut s = Snapshot::new();
        set(&mut s, reg::CURRENT, 0);
        assert_eq!(current(&s), None);
    }

    #[test]
    fn completeness_depends_on_what_is_polled() {
        let sample = fixture();
        let copy = |ranges: &[std::ops::Range<u16>]| {
            let mut s = Snapshot::new();
            for address in ranges.iter().flat_map(|r| r.clone()) {
                s.set(Space::Holding, address, sample.get(Space::Holding, address).unwrap_or(0));
            }
            s
        };
        assert!(is_complete(&sample, true));
        let without_cells = copy(&[0x1000..0x1010, 0x1400..0x1500]);
        assert!(is_complete(&without_cells, false) && !is_complete(&without_cells, true));
        // The last word of the serial number is missing.
        assert!(!is_complete(&copy(&[0x1000..0x1010, 0x1400..0x145F, 0x1460..0x14C0]), false));
    }

    #[test]
    fn invalid_values_are_reported_as_such() {
        let t = topology();
        let mut s = fixture();
        // Two states at once, and none.
        for word in [0x0C00, 0x0001] {
            set(&mut s, reg::STATE, word);
            assert!(invalid(state(&s)).contains("state"));
        }
        // 1000.01 A, and right at the limit.
        set(&mut s, reg::CURRENT, 1);
        set(&mut s, reg::CURRENT + 1, 0x86A1);
        assert_eq!(invalid(current(&s)), "more than 1000 A");
        assert_eq!(invalid(power(&s)), "more than 1000 A");
        set(&mut s, reg::CURRENT + 1, 0x86A0);
        assert_eq!((ok(current(&s)), ok(power(&s))), ("1000.00".into(), "383300".into()));
        set(&mut s, reg::CURRENT, 0xFFFE);
        set(&mut s, reg::CURRENT + 1, 0x7960);
        assert_eq!(ok(current(&s)), "-1000.00");

        for address in [reg::STATE_OF_CHARGE, reg::STATE_OF_HEALTH] {
            set(&mut s, address, 101);
            assert_eq!(invalid(percentage(&s, address)), "more than 100 %");
            set(&mut s, address, 100);
            assert_eq!(ok(percentage(&s, address)), "100");
        }

        // The module and cell indexes are 0-based.
        for (address, value, valid) in [
            (reg::MAX_TEMPERATURE_MODULE_INDEX, 3, true),
            (reg::MAX_TEMPERATURE_MODULE_INDEX, 4, false),
            (reg::MAX_TEMPERATURE_MODULE_INDEX, 0xFFFF, false),
        ] {
            set(&mut s, address, value);
            assert_eq!(module_number(&s, &t, address).unwrap().is_ok(), valid);
        }
        let cell = |s: &Snapshot, pick| cell_location(s, &t, reg::MAX_CELL_INDEX, pick);
        set(&mut s, reg::MAX_CELL_INDEX, 119);
        assert_eq!((ok(cell(&s, |m, _| m)), ok(cell(&s, |_, c| c))), ("4".into(), "30".into()));
        set(&mut s, reg::MAX_CELL_INDEX, 120);
        assert!(cell(&s, |m, _| m).unwrap().is_err());

        // Cell voltages of 0, and the highest below the lowest.
        set(&mut s, reg::MIN_CELL_VOLTAGE, 0);
        assert!(extreme_cell_voltage(&s, reg::MIN_CELL_VOLTAGE).unwrap().is_err());
        assert!(cell_voltage_spread(&s).unwrap().is_err());
        set(&mut s, reg::MIN_CELL_VOLTAGE, 3300);
        assert!(cell_voltage_spread(&s).unwrap().is_err());
        assert_eq!(ok(extreme_cell_voltage(&s, reg::MIN_CELL_VOLTAGE)), "3.300");

        // A module without voltage means that there is no average, either.
        let module_2 = MODULE_VOLTAGES.start + 2;
        set(&mut s, module_2, 0);
        assert!(average_cell_voltage(&s, &t).unwrap().is_err());
        assert!(module_voltage(&s, &t, 2).unwrap().is_err());
        assert!(module_average_cell_voltage(&s, &t, 2).unwrap().is_err());
        assert!(module_voltage(&s, &t, 1).unwrap().is_ok());
        set(&mut s, CELL_VOLTAGES.start + 31, 0);
        assert!(single_cell_voltage(&s, &t, 1, 1).unwrap().is_err());
        // Only the modules that are there matter for the average.
        set(&mut s, module_2, 9579);
        set(&mut s, MODULE_VOLTAGES.start + 4, 0);
        assert!(average_cell_voltage(&s, &t).unwrap().is_ok());

        for (address, inverted) in
            [(reg::CHARGE_NOT_ALLOWED, true), (reg::FORCE_CHARGE_REQUEST, false)]
        {
            for word in [2, 0xFFFF] {
                set(&mut s, address, word);
                assert_eq!(invalid(flag(&s, address, inverted)), "neither 0 nor 1");
            }
            set(&mut s, address, 1);
            assert_eq!(ok(flag(&s, address, inverted)), (!inverted).to_string());
        }
    }

    #[test]
    fn the_extremes_of_the_cells_of_a_module_are_the_first_to_have_them() {
        let t = topology();
        let mut s = fixture();
        let extremes = |s: &Snapshot, module| {
            let read = |text: fn(&CellExtremes) -> String| module_cells(s, &t, module, text);
            let all = [
                read(|e| e.max.0.to_string()),
                read(|e| e.max.1.to_string()),
                read(|e| e.min.0.to_string()),
                read(|e| e.min.1.to_string()),
            ];
            all.map(|reading| reading.map(|r| r.unwrap_or_else(|e| e.0.to_string())))
        };
        let cell = |module: u16, cell: u16| CELL_VOLTAGES.start + module * 30 + cell - 1;
        // The sample has 3190 to 3194 mV, over and over.
        let strs = |a: [Option<String>; 4]| a.map(Option::unwrap);
        assert_eq!(strs(extremes(&s, 0)), ["3194", "5", "3190", "1"]);

        for (n, mv) in [(12, 3300), (20, 3300), (7, 3100), (8, 3100)] {
            set(&mut s, cell(1, n), mv);
        }
        assert_eq!(strs(extremes(&s, 1)), ["3300", "12", "3100", "7"]);
        assert_eq!(strs(extremes(&s, 2)), ["3194", "5", "3190", "1"]);
        // Every cell the same: the first is both.
        for i in 1..=30 {
            set(&mut s, cell(2, i), 3200);
        }
        assert_eq!(strs(extremes(&s, 2)), ["3200", "1", "3200", "1"]);
        // The last cell of the module, and the first of the next.
        set(&mut s, cell(3, 30), 3400);
        set(&mut s, cell(3, 30) + 1, 3500);
        assert_eq!(strs(extremes(&s, 3))[1], "30");
        // A cell without a voltage, or without it known: no extremes for the module.
        set(&mut s, cell(1, 30), 0);
        assert_eq!(strs(extremes(&s, 1)), ["0 V"; 4]);
        let mut s = Snapshot::new();
        assert_eq!(strs(extremes(&s, 0)), ["a cell has not been read"; 4]);
        set(&mut s, cell(0, 1), 3200);
        assert_eq!(strs(extremes(&s, 0)), ["a cell has not been read"; 4]);
        assert_eq!(module_cells(&s, &t, 4, |_| String::new()), None);
    }

    #[test]
    fn topologies_make_sense_or_are_not_used() {
        for (modules, cells) in [(4, 120), (6, 150), (1, 1)] {
            assert!(Topology::new(modules, cells).is_ok());
        }
        assert_eq!(Topology::new(6, 150).unwrap().cells_per_module(), 25);
        for (modules, cells) in
            [(0, 120), (7, 140), (4, 0), (4, 151), (4, 122), (0, 0), (u16::MAX, u16::MAX)]
        {
            assert!(Topology::new(modules, cells).is_err(), "{modules} {cells}");
        }
    }

    #[test]
    fn health_bits_have_names_and_the_rest_are_numbered() {
        let mut s = fixture();
        let mut conditions_of = |address, bits, names| {
            set(&mut s, address, bits);
            ok(conditions(&s, address, names))
        };
        // Cell over-voltage, short circuit, the duplicate module over-temperature and bit 11, which
        // has no name.
        assert_eq!(
            conditions_of(reg::PROTECTIONS, 0b0100_1100_1010_0000, PROTECTION_BITS),
            r#"["bit-11","cell-over-voltage","module-over-temperature","short-circuit"]"#
        );
        assert_eq!(
            conditions_of(reg::ALARMS, 0x8400 | 0b0011_0000, ALARM_BITS),
            r#"["bit-10","bit-15","module-high-temperature","module-low-temperature"]"#
        );
        assert_eq!(conditions_of(reg::FAULTS, 0, FAULT_BITS), "[]");
        assert_eq!(
            conditions_of(reg::FAULTS, 0x0400 | 0x0200 | 0x0001 | 1 << 12, FAULT_BITS),
            r#"["bit-12","i2c-fault","initialisation-mismatch","voltage-sensing-fault"]"#
        );
        assert_eq!(
            conditions_of(reg::PROTECTIONS, 0xFFFF, PROTECTION_BITS).matches(',').count() + 1,
            14
        );

        for names in [PROTECTION_BITS, ALARM_BITS, FAULT_BITS] {
            let mut seen = std::collections::BTreeMap::new();
            for (bit, name) in names {
                assert!(*bit < 16);
                if let Some(other) = seen.insert(name, bit) {
                    assert!(other - bit == 2 && [4, 5].contains(bit), "{name}");
                }
            }
            assert!(names.windows(2).all(|w| w[0].0 > w[1].0));
        }
    }

    #[test]
    fn identification_has_to_be_proper_text() {
        let mut s = fixture();
        assert_eq!(name(&s).as_deref(), Some("Pylon Force_H2"));
        // Not printable, and then the model that is empty, and padded with spaces rather than NULs.
        set(&mut s, 0x1001, 0x6c01);
        assert_eq!(name(&s), None);
        set(&mut s, 0x1001, 0x6c6f);
        for (words, expected) in [([0, 0], None), ([0x4162, 0x2020], Some("Pylon Ab"))] {
            set(&mut s, 0x1005, words[0]);
            set(&mut s, 0x1006, words[1]);
            set(&mut s, 0x1007, 0);
            set(&mut s, 0x1008, 0);
            assert_eq!(name(&s).as_deref(), expected);
        }

        for (word, expected) in [
            (0x0102, Err("not printable ASCII")),
            (0xC3A9, Err("not printable ASCII")),
            (0, Err("empty")),
            (0x2020, Err("empty")),
            (0x4142, Ok("AB".repeat(16))),
        ] {
            for address in 0x1450..0x1460 {
                set(&mut s, address, word);
            }
            assert_eq!(serial_number(&s).unwrap().map_err(|e| e.0), expected);
        }
    }

    #[test]
    fn the_polling_plan_has_what_the_properties_need() {
        let period = Duration::from_secs(5);
        let plan = |detailed| {
            let blocks = poll_plan(period, detailed);
            blocks.iter().map(|b| (b.space, b.start, b.count, b.period)).collect::<Vec<_>>()
        };
        let constant = Duration::from_secs(600);
        let mut expected = vec![
            (Space::Holding, 0x1000, 12, constant),
            (Space::Holding, 0x1450, 16, constant),
            (Space::Holding, 0x1400, 58, period),
            (Space::Holding, 0x1460, 6, period),
            (Space::Holding, 0x14B0, 7, period),
        ];
        assert_eq!(plan(false), expected);
        expected.push((Space::Holding, 0x1500, 150, period));
        assert_eq!(plan(true), expected);
    }
}
