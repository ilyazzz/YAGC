use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, num::ParseIntError, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OcController {
    Old(OldController),
    New(NewController),
    Basic(BasicController),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasicPowerLevel {
    clockspeed: i64,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasicClocksTable {
    pub gpu_power_levels: BTreeMap<u32, BasicPowerLevel>,
    pub mem_power_levels: BTreeMap<u32, BasicPowerLevel>,
}

/// Used as a fallback when pp_od_clk_voltage is not availiable. Only supports enabling/disabling
/// the power states, not changing them
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BasicController {
    hw_path: PathBuf,
    clocks_table: BasicClocksTable,
}

impl BasicController {
    pub fn new(hw_path: PathBuf) -> Result<Self, OcControllerError> {
        let mut gpu_power_levels = BTreeMap::new();

        let pp_dpm_sclk = fs::read_to_string(hw_path.join("pp_dpm_sclk"))?;

        for line in pp_dpm_sclk.trim().split('\n') {
            log::trace!("Parsing pp_dpm_sclk line {}", &line);
            let mut split = line.split_whitespace();

            let num: u32 = split
                .next()
                .ok_or_else(|| {
                    OcControllerError::MissingValue("Missing pstate number".to_string())
                })?
                .strip_suffix(":")
                .ok_or_else(|| OcControllerError::MissingSuffix("Missing : suffix".to_string()))?
                .parse()?;
            let clockspeed: i64 = split
                .next()
                .ok_or_else(|| OcControllerError::MissingValue("Missing clockspeed".to_string()))?
                .strip_suffix("Mhz")
                .ok_or_else(|| OcControllerError::MissingSuffix("Missing Mhz".to_string()))?
                .parse()?;

            let power_level = BasicPowerLevel {
                clockspeed,
                enabled: true,
            };

            gpu_power_levels.insert(num, power_level);
        }

        let mut mem_power_levels = BTreeMap::new();

        let pp_dpm_mclk = fs::read_to_string(hw_path.join("pp_dpm_mclk"))?;

        for line in pp_dpm_mclk.trim().split('\n') {
            log::trace!("Parsing pp_dpm_mclk line {}", &line);
            let mut split = line.split_whitespace();

            let num: u32 = split
                .next()
                .ok_or_else(|| {
                    OcControllerError::MissingValue("Missing pstate number".to_string())
                })?
                .strip_suffix(":")
                .ok_or_else(|| OcControllerError::MissingSuffix("Missing : suffix".to_string()))?
                .parse()?;
            let clockspeed: i64 = split
                .next()
                .ok_or_else(|| OcControllerError::MissingValue("Missing clockspeed".to_string()))?
                .strip_suffix("Mhz")
                .ok_or_else(|| OcControllerError::MissingSuffix("Missing Mhz".to_string()))?
                .parse()?;

            let power_level = BasicPowerLevel {
                clockspeed,
                enabled: true,
            };

            mem_power_levels.insert(num, power_level);
        }

        let clocks_table = BasicClocksTable {
            gpu_power_levels,
            mem_power_levels,
        };

        Ok(Self {
            hw_path,
            clocks_table,
        })
    }

    pub fn get_table(&self) -> BasicClocksTable {
        self.clocks_table.clone()
    }

    pub fn set_gpu_power_levels(
        &mut self,
        levels: BTreeMap<u32, BasicPowerLevel>,
    ) -> Result<(), OcControllerError> {
        let levels_string: String = levels
            .iter()
            .filter(|l| l.1.enabled)
            .map(|l| *l.0)
            .map(|l| l.to_string())
            .collect::<Vec<String>>()
            .join(" ");

        log::info!("Writing {} to pp_dpm_sclk", levels_string);

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct OldClocksTable {
    pub gpu_power_levels: BTreeMap<u32, (i64, i64)>, //<power level, (clockspeed, voltage)>
    pub mem_power_levels: BTreeMap<u32, (i64, i64)>,
    pub gpu_clocks_range: (i64, i64),
    pub mem_clocks_range: (i64, i64),
    pub voltage_range: (i64, i64), //IN MILLIVOLTS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OldController {
    hw_path: PathBuf,
}

impl OldController {
    pub fn new(hw_path: PathBuf) -> Self {
        Self { hw_path }
    }

    // It probably doesn't make much sense taking both the pp_od_clk_voltage and the hardware path
    // where it's read from, but otherwise the file would have to be read twice (first in
    // GpuController to recognize the clocks table type
    pub fn get_table(&self) -> Result<OldClocksTable, OcControllerError> {
        let mut clocks_table = OldClocksTable::default();

        let pp_od_clk_voltage = fs::read_to_string(self.hw_path.join("pp_od_clk_voltage"))?;

        let mut lines_iter = pp_od_clk_voltage.trim().split("\n").into_iter();

        log::trace!("Reading clocks table");

        while let Some(line) = lines_iter.next() {
            let line = line.trim();
            log::trace!("Parsing line {}", line);

            match line {
                "OD_SCLK:" | "OD_MCLK:" => {
                    let is_vram = match line {
                        "OD_SCLK:" => false,
                        "OD_MCLK:" => true,
                        _ => unreachable!(),
                    };

                    log::trace!("Parsing clock levels");

                    // If `next()` is used on the main iterator directly, it will consume the `OD_MCLK:` aswell,
                    // which means the outer loop won't recognize that the next lines are of a different clock type.
                    // Thus, it is better to count how many lines were of the clock levels and then substract that amount from the main iterator.
                    let mut i = 0;
                    let mut lines = lines_iter.clone();

                    while let Some(line) = lines.next() {
                        let line = line.trim();
                        log::trace!("Parsing power level line {}", line);

                        // Probably shouldn't unwrap, will fail on empty lines in clocks table
                        if let Some(_) = line.chars().next().unwrap().to_digit(10) {
                            let (num, clock, voltage) = Self::parse_clock_voltage_line(line)?;

                            log::trace!("Power level {}: {}MHz {}mV", num, clock, voltage);

                            if is_vram {
                                clocks_table.mem_power_levels.insert(num, (clock, voltage));
                            } else {
                                clocks_table.gpu_power_levels.insert(num, (clock, voltage));
                            }

                            i += 1;
                        } else {
                            // Probably a better way to do this
                            for _ in 0..i {
                                lines_iter.next().unwrap();
                            }
                            log::trace!("Finished reading clock levels");
                            break;
                        }
                    }
                }
                "OD_RANGE:" => {
                    log::trace!("Parsing clock and voltage ranges");

                    while let Some(line) = lines_iter.next() {
                        let mut split = line.split_whitespace();

                        let name = split.next().ok_or_else(|| {
                            OcControllerError::MissingValue("get range name".to_string())
                        })?;
                        let min = split.next().ok_or_else(|| {
                            OcControllerError::MissingValue("range minimal value".to_string())
                        })?;
                        let max = split.next().ok_or_else(|| {
                            OcControllerError::MissingValue("range maximum value".to_string())
                        })?;

                        match name {
                            "SCLK:" => {
                                let min_clock: i64 = min.replace("MHz", "").parse()?;
                                let max_clock: i64 = max.replace("MHz", "").parse()?;

                                clocks_table.gpu_clocks_range = (min_clock, max_clock);
                            }
                            "MCLK:" => {
                                let min_clock: i64 = min.replace("MHz", "").parse()?;
                                let max_clock: i64 = max.replace("MHz", "").parse()?;

                                clocks_table.mem_clocks_range = (min_clock, max_clock);
                            }
                            "VDDC:" => {
                                let min_voltage: i64 = min.replace("mV", "").parse()?;
                                let max_voltage: i64 = max.replace("mV", "").parse()?;

                                clocks_table.voltage_range = (min_voltage, max_voltage);
                            }
                            _ => {
                                return Err(OcControllerError::UnknownValue(
                                    "unrecognized voltage range type".to_string(),
                                ))
                            }
                        }
                    }
                }
                _ => {
                    return Err(OcControllerError::UnknownValue(
                        "unrecognized line type".to_string(),
                    ))
                }
            }
        }

        log::trace!("Successfully parsed the clocks table");
        Ok(clocks_table)
    }

    fn parse_clock_voltage_line(line: &str) -> Result<(u32, i64, i64), OcControllerError> {
        log::trace!("Parsing line {}", line);

        let line = line.to_uppercase();
        let line_parts: Vec<&str> = line.split_whitespace().collect();

        let num: u32 = line_parts
            .get(0)
            .ok_or_else(|| OcControllerError::MissingValue("power level number".to_string()))?
            .chars()
            .nth(0)
            .unwrap()
            .to_digit(10)
            .unwrap();
        let clock: i64 = line_parts
            .get(1)
            .ok_or_else(|| OcControllerError::MissingValue("clockspeed".to_string()))?
            .strip_suffix("MHZ")
            .ok_or_else(|| OcControllerError::MissingSuffix("\"MHZ\"".to_string()))?
            .parse()?;
        let voltage: i64 = line_parts
            .get(2)
            .ok_or_else(|| OcControllerError::MissingValue("voltage".to_string()))?
            .strip_suffix("MV")
            .ok_or_else(|| OcControllerError::MissingSuffix("\"mV\"".to_string()))?
            .parse()?;

        Ok((num, clock, voltage))
    }

    pub fn set_gpu_power_state(
        &self,
        num: u32,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), io::Error> {
        let mut line = format!("s {} {}", num, clockspeed);

        if let Some(voltage) = voltage {
            line.push_str(&format!(" {}", voltage));
        }
        line.push_str("\n");

        log::info!("Setting gpu power state {}", line);
        log::info!("Writing {} to pp_od_clk_voltage", line);

        fs::write(self.hw_path.join("pp_od_clk_voltage"), line)?;

        Ok(())
    }

    pub fn set_vram_power_state(
        &self,
        num: u32,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), io::Error> {
        let mut line = format!("m {} {}", num, clockspeed);

        if let Some(voltage) = voltage {
            line.push_str(&format!(" {}", voltage));
        }
        line.push_str("\n");

        log::info!("Setting vram power state {}", line);
        log::info!("Writing {} to pp_od_clk_voltage", line);

        fs::write(self.hw_path.join("pp_od_clk_voltage"), line)?;

        Ok(())
    }

    pub fn commit_gpu_power_states(&mut self) -> Result<(), OcControllerError> {
        fs::write(self.hw_path.join("pp_od_clk_voltage"), b"c\n")?;
        Ok(())
    }

    pub fn reset_gpu_power_states(&mut self) -> Result<(), OcControllerError> {
        fs::write(self.hw_path.join("pp_od_clk_voltage"), b"r\n")?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct NewClocksTable {
    pub current_gpu_clocks: (i64, i64),
    pub current_max_mem_clock: i64,
    // pub vddc_curve: [(i64, i64); 3],
    pub gpu_clocks_range: (i64, i64),
    pub mem_clocks_range: (i64, i64),
    // pub voltage_range: (i64, i64), //IN MILLIVOLTS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewController {
    hw_path: PathBuf,
}

impl NewController {
    pub fn new(hw_path: PathBuf) -> Self {
        Self { hw_path }
    }

    pub fn parse(&self) -> Result<NewClocksTable, OcControllerError> {
        log::trace!("Detected clocks table format for Vega20 or newer");

        let pp_od_clk_voltage = fs::read_to_string(self.hw_path.join("pp_od_clk_voltage"))?;

        let mut clocks_table = NewClocksTable::default();

        let mut lines_iter = pp_od_clk_voltage.trim().split("\n").into_iter();

        log::trace!("Reading clocks table");

        while let Some(line) = &lines_iter.next() {
            let line = line.trim();
            log::trace!("Parsing line {}", line);

            match line {
                "OD_SCLK:" => {
                    let min_clock_line = lines_iter
                        .next()
                        .ok_or_else(|| {
                            OcControllerError::MissingValue(
                                "unexpected clocks file end".to_string(),
                            )
                        })?
                        .trim()
                        .to_lowercase();

                    let min_clock: i64 = min_clock_line
                        .strip_prefix("0:")
                        .ok_or_else(|| {
                            OcControllerError::InvalidValue(format!(
                                "invalid clock line prefix in {}",
                                min_clock_line
                            ))
                        })?
                        .strip_suffix("mhz")
                        .ok_or_else(|| {
                            OcControllerError::InvalidValue(format!(
                                "invalid clock line suffix in {}",
                                min_clock_line
                            ))
                        })?
                        .trim()
                        .parse()?;

                    let max_clock_line = lines_iter
                        .next()
                        .ok_or_else(|| {
                            OcControllerError::InvalidValue(
                                "unexpeceted clocks file end".to_string(),
                            )
                        })?
                        .trim()
                        .to_lowercase();

                    let max_clock: i64 = max_clock_line
                        .strip_prefix("1:")
                        .ok_or_else(|| {
                            OcControllerError::MissingSuffix("missing pstate number".to_string())
                        })?
                        .strip_suffix("mhz")
                        .ok_or_else(|| OcControllerError::MissingSuffix("missing mhz".to_string()))?
                        .trim()
                        .parse()?;

                    clocks_table.current_gpu_clocks = (min_clock, max_clock);
                }
                "OD_MCLK:" => {
                    let max_clock_line = lines_iter
                        .next()
                        .ok_or_else(|| {
                            OcControllerError::MissingValue(
                                "unexpected clocks file end".to_string(),
                            )
                        })?
                        .trim()
                        .to_lowercase();

                    let max_clock = max_clock_line
                        .strip_prefix("1:")
                        .ok_or_else(|| {
                            OcControllerError::MissingPrefix(format!(
                                "invalid clock line prefix in {}",
                                max_clock_line
                            ))
                        })?
                        .strip_suffix("mhz")
                        .ok_or_else(|| {
                            OcControllerError::MissingSuffix(format!(
                                "invalid clock line suffix in {}",
                                max_clock_line
                            ))
                        })?
                        .trim()
                        .parse()?;

                    clocks_table.current_max_mem_clock = max_clock;
                }
                "OD_RANGE:" => {
                    while let Some(line) = &lines_iter.next() {
                        let line = line.trim();
                        log::trace!("Parsing OD_RANGE line {}", &line);

                        match &line[..5] {
                            "SCLK:" => {
                                let mut split = line.split_whitespace();

                                // Skips the 'SCLK'
                                split.next().unwrap();

                                let min_clock = split
                                    .next()
                                    .unwrap()
                                    .strip_suffix("Mhz")
                                    .ok_or_else(|| {
                                        OcControllerError::MissingSuffix("missing MHz".to_string())
                                    })?
                                    .parse()?;

                                let max_clock = split
                                    .next()
                                    .unwrap()
                                    .strip_suffix("Mhz")
                                    .ok_or_else(|| {
                                        OcControllerError::MissingSuffix("missing MHz".to_string())
                                    })?
                                    .parse()?;

                                clocks_table.gpu_clocks_range = (min_clock, max_clock);
                            }
                            "MCLK:" => {
                                let mut split = line.split_whitespace();

                                // Skips the 'SCLK'
                                split.next().unwrap();

                                let min_clock = split
                                    .next()
                                    .unwrap()
                                    .strip_suffix("Mhz")
                                    .ok_or_else(|| {
                                        OcControllerError::MissingSuffix("missing MHz".to_string())
                                    })?
                                    .parse()?;

                                let max_clock = split
                                    .next()
                                    .unwrap()
                                    .strip_suffix("Mhz")
                                    .ok_or_else(|| {
                                        OcControllerError::MissingSuffix("missing MHz".to_string())
                                    })?
                                    .parse()?;

                                clocks_table.mem_clocks_range = (min_clock, max_clock);
                            }
                            _ => {
                                log::trace!("OD_RANGE ended");
                                break;
                            }
                        }
                    }
                }
                _ => {
                    log::trace!("Skipping line");
                    continue;
                }
            }
        }

        Ok(clocks_table)
    }

    pub fn set_gpu_max_state(
        &self,
        clockspeed: i64,
        voltage: Option<i64>,
    ) -> Result<(), OcControllerError> {
        let s_line = format!("s 1 {}\n", clockspeed);

        fs::write(self.hw_path.join("pp_od_clk_voltage"), s_line)?;

        if let Some(voltage) = voltage {
            let vc_line = format!("vc 2 {} {}\n", clockspeed, voltage);

            fs::write(self.hw_path.join("pp_od_clk_voltage"), vc_line)?;
        }

        Ok(())
    }

    pub fn set_vram_max_clockspeed(&self, clockspeed: i64) -> Result<(), OcControllerError> {
        let s_line = format!("m 1 {}\n", clockspeed);

        fs::write(self.hw_path.join("pp_od_clk_voltage"), s_line)?;

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum OcControllerError {
    MissingValue(String),
    MissingSuffix(String),
    MissingPrefix(String),
    UnknownValue(String),
    InvalidValue(String),
    IoError(String),
}

impl From<ParseIntError> for OcControllerError {
    fn from(err: ParseIntError) -> OcControllerError {
        OcControllerError::InvalidValue(err.to_string())
    }
}

impl From<io::Error> for OcControllerError {
    fn from(err: io::Error) -> OcControllerError {
        OcControllerError::IoError(err.to_string())
    }
}

#[derive(Clone)]
pub enum ClocksTable {
    New(NewClocksTable),
    Old(OldClocksTable),
    Basic(BasicClocksTable),
}
