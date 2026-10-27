//! The nodes and the properties of the stack device, named after what they mean to the user. The
//! nodes of the modules and the cells depend on how the battery is put together and are made as
//! needed.

use crate::battery::{Num::*, register::*, *};
use crate::modbus::Snapshot;
use homie5::HomieID;
use homie5::device_description::{
    DeviceDescriptionBuilder, HomieDeviceDescription, HomieNodeDescription,
    HomiePropertyDescription, NodeDescriptionBuilder, PropertyDescriptionBuilder,
};

/// What the stack is called if the battery does not say.
pub(crate) const FALLBACK_NAME: &str = "Pylontech Force H2";

#[derive(Clone, Copy)]
enum Kind {
    Float(&'static str),
    Percent,
    Count,
    /// A position, counted from 1.
    Position,
    Boolean,
    Text,
    State,
    Conditions,
}
use Kind::*;

const VOLTS: Kind = Float(homie5::HOMIE_UNIT_VOLT);
const AMPERES: Kind = Float(homie5::HOMIE_UNIT_AMPERE);
const WATTS: Kind = Float(homie5::HOMIE_UNIT_WATT);
const KILOWATT_HOURS: Kind = Float(homie5::HOMIE_UNIT_KILOWATTHOUR);
const CELSIUS: Kind = Float(homie5::HOMIE_UNIT_DEGREE_CELSIUS);

impl Kind {
    fn describe(self) -> HomiePropertyDescription {
        use PropertyDescriptionBuilder as B;
        match self {
            Float(unit) => B::float().unit(unit).build(),
            Percent => B::integer().range(0..=100).unit(homie5::HOMIE_UNIT_PERCENT).build(),
            Count => B::integer().unit(homie5::HOMIE_UNIT_COUNT_AMOUNT).build(),
            Position => B::integer().build(),
            Boolean => B::boolean().build(),
            Text => B::string().build(),
            State => B::enumeration(["idle", "charging", "discharging", "sleep"])
                .expect("enum values are valid")
                .build(),
            Conditions => {
                B::json().json_schema(r#"{"type":"array","items":{"type":"string"}}"#).build()
            }
        }
    }
}

type Reader = Box<dyn Fn(&Snapshot) -> Reading>;

/// How the value of a property of the stack is made out of the registers.
#[derive(Clone, Copy)]
enum Read {
    Plain(fn(&Snapshot) -> Reading),
    /// There is no such property if it is not known how the battery is put together.
    Topo(fn(&Snapshot, &Topology) -> Reading),
}
use Read::*;

struct Spec<R> {
    id: HomieID,
    kind: Kind,
    read: R,
}

impl<R> Spec<R> {
    const fn new(id: &'static str, kind: Kind, read: R) -> Self {
        Self { id: HomieID::new_const(id), kind, read }
    }
}

struct NodeSpec {
    id: HomieID,
    name: &'static str,
    properties: &'static [Spec<Read>],
}

impl NodeSpec {
    const fn new(id: &'static str, name: &'static str, properties: &'static [Spec<Read>]) -> Self {
        Self { id: HomieID::new_const(id), name, properties }
    }
}

type S = Spec<Read>;

static STACK: [S; 12] = [
    S::new("state", State, Plain(state)),
    S::new("voltage", VOLTS, Plain(|s| number(s, VOLTAGE, U16, 1))),
    S::new("current", AMPERES, Plain(current)),
    S::new("power", WATTS, Plain(power)),
    S::new("state-of-charge", Percent, Plain(|s| percentage(s, STATE_OF_CHARGE))),
    S::new("state-of-health", Percent, Plain(|s| percentage(s, STATE_OF_HEALTH))),
    S::new("remaining-energy", KILOWATT_HOURS, Plain(|s| number(s, REMAINING_ENERGY, U32, 3))),
    S::new("cycles", Count, Plain(|s| number(s, CYCLES, U16, 0))),
    S::new("max-temperature", CELSIUS, Plain(|s| number(s, MAX_MODULE_TEMPERATURE, I16, 1))),
    S::new("min-temperature", CELSIUS, Plain(|s| number(s, MIN_MODULE_TEMPERATURE, I16, 1))),
    S::new(
        "max-temperature-module",
        Position,
        Topo(|s, t| module_number(s, t, MAX_TEMPERATURE_MODULE_INDEX)),
    ),
    S::new(
        "min-temperature-module",
        Position,
        Topo(|s, t| module_number(s, t, MIN_TEMPERATURE_MODULE_INDEX)),
    ),
];

static CELLS: [S; 8] = [
    S::new("max-voltage", VOLTS, Plain(|s| extreme_cell_voltage(s, MAX_CELL_VOLTAGE))),
    S::new("min-voltage", VOLTS, Plain(|s| extreme_cell_voltage(s, MIN_CELL_VOLTAGE))),
    S::new("voltage-spread", VOLTS, Plain(cell_voltage_spread)),
    S::new("average-voltage", VOLTS, Topo(average_cell_voltage)),
    S::new(
        "max-voltage-module",
        Position,
        Topo(|s, t| cell_location(s, t, MAX_CELL_INDEX, |module, _| module)),
    ),
    S::new(
        "max-voltage-cell",
        Position,
        Topo(|s, t| cell_location(s, t, MAX_CELL_INDEX, |_, cell| cell)),
    ),
    S::new(
        "min-voltage-module",
        Position,
        Topo(|s, t| cell_location(s, t, MIN_CELL_INDEX, |module, _| module)),
    ),
    S::new(
        "min-voltage-cell",
        Position,
        Topo(|s, t| cell_location(s, t, MIN_CELL_INDEX, |_, cell| cell)),
    ),
];

static LIMITS: [S; 8] = [
    S::new("charge-voltage", VOLTS, Plain(|s| number(s, CHARGE_VOLTAGE, U16, 1))),
    S::new("charge-current", AMPERES, Plain(|s| number(s, CHARGE_CURRENT, I32, 2))),
    S::new("discharge-voltage", VOLTS, Plain(|s| number(s, DISCHARGE_VOLTAGE, U16, 1))),
    S::new("discharge-current", AMPERES, Plain(|s| number(s, DISCHARGE_CURRENT, AbsI32, 2))),
    S::new("charge-allowed", Boolean, Plain(|s| flag(s, CHARGE_NOT_ALLOWED, true))),
    S::new("discharge-allowed", Boolean, Plain(|s| flag(s, DISCHARGE_NOT_ALLOWED, true))),
    S::new("force-charge-request", Boolean, Plain(|s| flag(s, FORCE_CHARGE_REQUEST, false))),
    S::new("balance-charge-request", Boolean, Plain(|s| flag(s, BALANCE_CHARGE_REQUEST, false))),
];

// Wh are kWh with three decimals.
static ENERGY: [S; 4] = [
    S::new("charged-today", KILOWATT_HOURS, Plain(|s| number(s, CHARGED_TODAY, U32, 3))),
    S::new("discharged-today", KILOWATT_HOURS, Plain(|s| number(s, DISCHARGED_TODAY, U32, 3))),
    S::new("charged-total", KILOWATT_HOURS, Plain(|s| number(s, CHARGED_TOTAL, U32, 0))),
    S::new("discharged-total", KILOWATT_HOURS, Plain(|s| number(s, DISCHARGED_TOTAL, U32, 0))),
];

static HEALTH: [S; 3] = [
    S::new("protections", Conditions, Plain(|s| conditions(s, PROTECTIONS, PROTECTION_BITS))),
    S::new("alarms", Conditions, Plain(|s| conditions(s, ALARMS, ALARM_BITS))),
    S::new("faults", Conditions, Plain(|s| conditions(s, FAULTS, FAULT_BITS))),
];

static BMS: [S; 3] = [
    S::new("temperature", CELSIUS, Plain(|s| number(s, BMS_TEMPERATURE, I16, 1))),
    S::new("firmware-version", Text, Plain(firmware_version)),
    S::new("serial-number", Text, Plain(serial_number)),
];

static NODES: [NodeSpec; 6] = [
    NodeSpec::new("stack", "Stack", &STACK),
    NodeSpec::new("cells", "Cells", &CELLS),
    NodeSpec::new("limits", "Charge and discharge limits", &LIMITS),
    NodeSpec::new("energy", "Energy", &ENERGY),
    NodeSpec::new("health", "Health", &HEALTH),
    NodeSpec::new("bms", "Battery management system", &BMS),
];

type ModuleRead = fn(&Snapshot, &Topology, u16) -> Reading;

static MODULE: [Spec<ModuleRead>; 3] = [
    Spec::new("voltage", VOLTS, module_voltage),
    Spec::new("average-cell-voltage", VOLTS, module_average_cell_voltage),
    Spec::new("temperature", CELSIUS, module_temperature),
];

/// The properties of the nodes of the modules that are made out of the voltages of the cells.
static MODULE_CELLS: [Spec<ModuleRead>; 5] = [
    Spec::new("max-cell-voltage", VOLTS, |s, t, m| {
        module_cells(s, t, m, |e| fixed(e.max.0.into(), 3))
    }),
    Spec::new("min-cell-voltage", VOLTS, |s, t, m| {
        module_cells(s, t, m, |e| fixed(e.min.0.into(), 3))
    }),
    Spec::new("max-cell-voltage-cell", Position, |s, t, m| {
        module_cells(s, t, m, |e| e.max.1.to_string())
    }),
    Spec::new("min-cell-voltage-cell", Position, |s, t, m| {
        module_cells(s, t, m, |e| e.min.1.to_string())
    }),
    Spec::new("cell-voltage-spread", VOLTS, |s, t, m| {
        module_cells(s, t, m, |e| fixed(i64::from(e.max.0) - i64::from(e.min.0), 3))
    }),
];

/// A property of the stack device, and how to make its value out of the registers.
pub(crate) struct Property {
    pub node: HomieID,
    pub id: HomieID,
    pub read: Reader,
}

pub(crate) struct Layout {
    pub description: HomieDeviceDescription,
    pub properties: Vec<Property>,
}

#[derive(Default)]
struct Builder {
    nodes: Vec<(HomieID, HomieNodeDescription)>,
    properties: Vec<Property>,
}

impl Builder {
    fn node(
        &mut self,
        id: HomieID,
        name: String,
        properties: impl IntoIterator<Item = (HomieID, Kind, Reader)>,
    ) {
        let mut node = NodeDescriptionBuilder::new().name(name);
        for (property, kind, read) in properties {
            node = node.add_property(property.clone(), kind.describe());
            self.properties.push(Property { node: id.clone(), id: property, read });
        }
        self.nodes.push((id, node.build()));
    }
}

/// How the stack is described given the name of the battery, how it is put together (if that is
/// known) and whether the voltages of the cells are exposed.
pub(crate) fn layout(
    name: &str,
    topology: Option<Topology>,
    detailed: bool,
    bridge: &HomieID,
) -> Layout {
    let mut builder = Builder::default();
    for node in &NODES {
        let properties = node.properties.iter().filter_map(|spec| {
            let read: Reader = match (spec.read, topology) {
                (Plain(read), _) => Box::new(read),
                (Topo(read), Some(topology)) => Box::new(move |s| read(s, &topology)),
                (Topo(_), None) => return None,
            };
            Some((spec.id.clone(), spec.kind, read))
        });
        builder.node(node.id.clone(), node.name.to_string(), properties);
    }
    if let Some(topology) = topology {
        let cells = if detailed { &MODULE_CELLS[..] } else { &[] };
        for module in 1..=topology.modules {
            let id = HomieID::try_from(format!("module-{module}")).expect("a valid ID");
            let properties = MODULE.iter().chain(cells).map(|spec| {
                let read = spec.read;
                let read: Reader = Box::new(move |s| read(s, &topology, module - 1));
                (spec.id.clone(), spec.kind, read)
            });
            builder.node(id, format!("Module {module}"), properties);
        }
        for module in (1..=topology.modules).take(if detailed { usize::MAX } else { 0 }) {
            for cell in 1..=topology.cells_per_module() {
                let id = format!("module-{module}-cell-{cell}");
                let id = HomieID::try_from(id).expect("a valid ID");
                let read: Reader =
                    Box::new(move |s| single_cell_voltage(s, &topology, module - 1, cell - 1));
                let voltage = (HomieID::new_const("voltage"), VOLTS, read);
                builder.node(id, format!("Module {module} cell {cell}"), [voltage]);
            }
        }
    }
    let description = DeviceDescriptionBuilder::new().name(name).root(bridge.clone());
    let description =
        builder.nodes.into_iter().fold(description, |d, (id, node)| d.add_node(id, node));
    Layout { description: description.build(), properties: builder.properties }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::tests::{fixture, topology};

    #[test]
    fn the_fixture_is_decoded() {
        let bridge = HomieID::new_const("pylonforce");
        let full = layout("Battery", Some(topology()), true, &bridge);
        let sample = fixture();
        let read = |p: &Property| (p.read)(&sample).expect("has been read").expect("is valid");
        let values: std::collections::BTreeMap<_, _> =
            full.properties.iter().map(|p| (format!("{}/{}", p.node, p.id), read(p))).collect();
        assert_eq!(values.len(), 12 + 8 + 8 + 4 + 3 + 3 + 4 * 8 + 120);
        for (path, value) in [
            ("stack/state", "discharging"),
            ("stack/voltage", "383.3"),
            ("stack/current", "-0.11"),
            ("stack/power", "-42"),
            ("stack/state-of-charge", "18"),
            ("stack/state-of-health", "98"),
            ("stack/remaining-energy", "3.100"),
            ("stack/cycles", "123"),
            ("stack/max-temperature", "20.0"),
            ("stack/min-temperature", "19.0"),
            ("stack/max-temperature-module", "4"),
            ("stack/min-temperature-module", "1"),
            ("cells/max-voltage", "3.202"),
            ("cells/min-voltage", "3.188"),
            ("cells/voltage-spread", "0.014"),
            ("cells/average-voltage", "3.1943"),
            ("cells/max-voltage-module", "2"),
            ("cells/max-voltage-cell", "27"),
            ("cells/min-voltage-module", "2"),
            ("cells/min-voltage-cell", "18"),
            ("limits/charge-voltage", "432.0"),
            ("limits/charge-current", "18.50"),
            ("limits/discharge-voltage", "348.0"),
            ("limits/discharge-current", "18.50"),
            ("limits/charge-allowed", "true"),
            ("limits/discharge-allowed", "true"),
            ("limits/force-charge-request", "false"),
            ("limits/balance-charge-request", "false"),
            ("energy/charged-today", "1.234"),
            ("energy/discharged-today", "5.678"),
            ("energy/charged-total", "2100"),
            ("energy/discharged-total", "1900"),
            ("health/protections", "[]"),
            ("health/alarms", "[]"),
            ("health/faults", "[]"),
            ("bms/temperature", "32.0"),
            ("bms/firmware-version", "2.0 (B52.34)"),
            ("bms/serial-number", "PFH2TEST00000001"),
            ("module-1/voltage", "95.83"),
            ("module-4/voltage", "95.79"),
            ("module-1/average-cell-voltage", "3.1943"),
            ("module-4/temperature", "20.0"),
            ("module-1/max-cell-voltage", "3.194"),
            ("module-1/max-cell-voltage-cell", "5"),
            ("module-1/min-cell-voltage", "3.190"),
            ("module-1/min-cell-voltage-cell", "1"),
            ("module-4/cell-voltage-spread", "0.004"),
            ("module-1-cell-1/voltage", "3.190"),
            ("module-2-cell-27/voltage", "3.191"),
            ("module-4-cell-30/voltage", "3.194"),
        ] {
            assert_eq!(values[path], value, "{path}");
        }
        let unknown = layout("Battery", None, true, &bridge);
        let paths: Vec<_> =
            unknown.properties.iter().map(|p| format!("{}/{}", p.node, p.id)).collect();
        assert_eq!(paths.len(), 12 + 8 + 8 + 4 + 3 + 3 - 2 - 5);
        assert!(!paths.contains(&"cells/average-voltage".to_string()));
    }
}
