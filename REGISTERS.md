# Pylontech Force H2 Modbus registers

Modbus RTU, unit id 1 (or the DIP-switch address). Only function 0x03 (read holding registers) is useful; 0x04 gets
exception 01. A read must lie entirely within one of the windows 0x1000-0x108F, 0x1100-0x11FF or 0x1400-0x1AFF,
otherwise it gets exception 02. Unmapped addresses inside a window read 0. Read at most 125 registers at a time. A
request with a bad CRC is answered with exception 03.

All registers are big-endian. 32-bit values take two registers, high word first. ASCII holds two characters per
register, the first in the high byte, padded with NULs. Indices are 0-based; cell `i` is cell `i % 30` of module
`i / 30`. BMS (on Force-H2) supports at most 6 modules and 150 cells.

## Identification

| Addr | Type | Meaning |
|---|---|---|
| 0x1000-0x1004 | ASCII | Manufacturer, `Pylon` |
| 0x1005-0x1009 | ASCII | Model, `Force_H2` |
| 0x100A | u16 | Firmware version: major << 8 \| minor (`V2.0` = 0x0200) |
| 0x100B | u16 | Firmware build: the first two parts of `B52.34.0` as 52 << 8 \| 34 |
| 0x100C | u16 | 0 |

## Status

0x1100-0x1139 is an alias of 0x1400-0x1439.

| Addr | Type | Unit | Meaning |
|---|---|---|---|
| 0x1400 | u16 bits | | State, see below |
| 0x1401 | u16 bits | | Protections, see below |
| 0x1402 | u16 bits | | Alarms, see below |
| 0x1403 | u16 | 0.1 V | Stack voltage (sum of the cell voltages) |
| 0x1404-05 | i32 | 0.01 A | Stack current, positive when charging |
| 0x1406 | i16 | 0.1 °C | BMS board temperature (not a cell temperature) |
| 0x1407 | u16 | % | State of charge |
| 0x1408 | u16 | | Cycle count |
| 0x1409 | u16 | 0.1 V | Charge cut-off voltage |
| 0x140A-0B | i32 | 0.01 A | Charge current limit |
| 0x140C | u16 | 0.1 V | Discharge cut-off voltage |
| 0x140D-0E | i32 | 0.01 A | Discharge current limit, negative |
| 0x140F | u16 bits | | Latched flags; bits 0 and 1 set together when there are no faults, bit 3 relay fault, bits 4-6 unknown |
| 0x1410 | u16 | mV | Maximum cell voltage |
| 0x1411 | u16 | mV | Minimum cell voltage |
| 0x1412 | u16 | | Index of the cell with the maximum voltage |
| 0x1413 | u16 | | Index of the cell with the minimum voltage |
| 0x1414 | i16 | 0.1 °C | Maximum cell temperature |
| 0x1415 | i16 | 0.1 °C | Minimum cell temperature |
| 0x1416 | u16 | | Index of the cell with the maximum temperature |
| 0x1417 | u16 | | Index of the cell with the minimum temperature |
| 0x1418 | u16 | 0.01 V | Maximum module voltage |
| 0x1419 | u16 | 0.01 V | Minimum module voltage |
| 0x141A | u16 | | Index of the module with the maximum voltage |
| 0x141B | u16 | | Index of the module with the minimum voltage |
| 0x141C | i16 | 0.1 °C | Maximum module temperature |
| 0x141D | i16 | 0.1 °C | Minimum module temperature |
| 0x141E | u16 | | Index of the module with the maximum temperature |
| 0x141F | u16 | | Index of the module with the minimum temperature |
| 0x1420 | u16 | % | State of health |
| 0x1421-22 | u32 | Wh | Remaining energy, including the reserve below the depth-of-discharge limit |
| 0x1423-24 | u32 | Wh | Charged today, as of the last 5-minute snapshot |
| 0x1425-26 | u32 | Wh | Discharged today, as of the last 5-minute snapshot |
| 0x1427-28 | u32 | Wh | Charged today; resets at midnight by the BMS clock |
| 0x1429-2A | u32 | Wh | Discharged today |
| 0x142B-2C | u32 | kWh | Charged in total; unreliable, jumps by large amounts |
| 0x142D-2E | u32 | kWh | Discharged in total; unreliable, jumps by large amounts |
| 0x142F | u16 | 0/1 | Force charge request |
| 0x1430 | u16 | 0/1 | Balance charge request |
| 0x1431-32 | | | 0 |
| 0x1433 | u16 bits | | Faults, see below |
| 0x1434-35 | | | 0 |
| 0x1436 | u16 | | Number of modules |
| 0x1437 | u16 | | Number of cells |
| 0x1438 | u16 | 0/1 | Charging not allowed (charge current limit is 0) |
| 0x1439 | u16 | 0/1 | Discharging not allowed (discharge current limit is 0) |
| 0x1450-0x145F | ASCII | | Serial number (16 characters, the rest empty) |

## Modules and cells

Entries past the number of modules or cells read 0.

| Addr | Type | Unit | Meaning |
|---|---|---|---|
| 0x1460 + m | u16 | 0.01 V | Module voltage (sum of its cell voltages) |
| 0x14B0 + m | i16 | 0.1 °C | Module temperature |
| 0x1500 + i | u16 | mV | Cell voltage |
| 0x1800 + i | i16 | 0.1 °C | Cell temperature: the module temperature, in whole degrees, repeated for each of its cells |

## Bitfields

### 0x1400 state

| Bit | Meaning |
|---|---|
| 0 | Charging possible (set when idle or charging) |
| 1 | Discharging possible (set when idle or discharging) |
| 3 | Any fault |
| 4 | Current protection |
| 5 | Voltage protection |
| 6 | Temperature protection |
| 7 | Voltage alarm |
| 8 | Current alarm |
| 9 | Temperature alarm |
| 10 | Idle (also powered off or in system error) |
| 11 | Charging |
| 12 | Discharging |
| 13 | Sleeping |

### 0x1401 protections and 0x1402 alarms

| Bit | 0x1401 | 0x1402 |
|---|---|---|
| 15 | Module shutdown voltage | |
| 14 | Cell over-voltage | Cell high voltage |
| 13 | Cell under-voltage or shutdown voltage | Cell low voltage |
| 12 | Stack or cell over-temperature | Cell high temperature |
| 11 | | Stack high temperature |
| 10 | Short circuit | |
| 9 | Discharge over-current | Discharge over-current |
| 8 | Charge over-current | Charge over-current |
| 7 | Module over-temperature | Module high temperature |
| 6 | Module under-temperature | Module low temperature |
| 5 | Module over-temperature (same as bit 7) | Module high temperature (same as bit 7) |
| 4 | Module under-temperature (same as bit 6) | Module low temperature (same as bit 6) |
| 3 | Stack over-voltage | Stack high voltage |
| 2 | Stack under-voltage or shutdown voltage | Stack low voltage |
| 1 | Module over-voltage | Module high voltage |
| 0 | Module under-voltage or shutdown voltage | Module low voltage |

### 0x1433 faults

| Bit | Meaning |
|---|---|
| 10 | Initialisation mismatch (voltage, cell count, coulomb counter or module) |
| 9 | I²C error |
| 8 | Battery monitor IC error |
| 7 | Short circuit |
| 6 | Battery damaged |
| 5 | Relay error or welded relay |
| 4 | Reverse voltage |
| 3 | DC over-voltage |
| 2 | Module offline |
| 1 | Temperature sensing error |
| 0 | Voltage sensing error |
