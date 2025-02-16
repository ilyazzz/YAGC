# Configuration

The LACT config file is located in `/etc/lact/config.yaml`, and contains all of the GPU settings that are typically edited in the GUI, as well as a few settings specifying the behaviour of the daemon.
LACT listens for config file changes and reloads all GPU settings automatically, but daemon-related settings such as the logging level or permissions require a service restart (`systemctl restart lactd`).

Full config file with all possible options:
```yaml
# WARNING: this is only an example of each possible setting. DO NOT COPY THIS CONFIG AS IS.
# Many options don't make sense to be used together, and depend on your hardware.
daemon:
  # The logging level of the daemon.
  # Possible values: `error`, `warn`, `info` (default), `debug`, `trace`
  log_level: info
  # User groups who should have access to the daemon.
  # WARNING: only the first group from this list that is found on the system is used!
  # This is made a list and not a single value to allow this config to work across 
  # different distros, which might have different groups for an "admin" user.
  admin_groups:
  - wheel
  - sudo
  # If set to `true`, this setting makes the LACT daemon not reset
  # GPU clocks when changing other settings or when turning off the daemon.
  # Can be used to work around a few very specific issues with 
  # some settings not applying on AMD GPUs.
  disable_clocks_cleanup: false
  # Daemon's TCP listening address. Not specified by default.
  # By default TCP access is disabled, and only a unix socket is present.
  # Specifying this option enables the TCP listener.
  tcp_listen_address: 127.0.0.1:12853
  # Prometheus metrics exporter listening address.
  # When not specified (which is the default), the exporter is disabled.
  exporter_listen_address: 127.0.0.1:9091

# Period in seconds for how long settings should wait to be confirmed.
# Most GPU setting change commands require a confirmation command to be used
# in order to save these settings to the config. 
# If a confirm command is not issued within the configured period (default: 5 seconds)
# the setting will be reverted.
apply_settings_timer: 5

# The main GPU configuration map, containing the list of GPUs and their settings.
gpus:
  # A GPU config entry. This is the ID of the GPU.
  # The ID is formed with a combination of a PCI device id, 
  # PCI subsystem id and PCI slot name to uniquely identify 
  # each GPU in the system, even if there are multiple of the same model.

  # You can discover the id of your GPU by either:
  # - Changing a setting in the UI, so it's written to the config
  # - Using `lact cli list-gpus`
  1002:687F-1043:0555-0000:0b:00.0:
    # Whether the daemon should touch fan control settings at all.
    # Setting this to `true` requires the `fan_control_settings` field to be present as well.
    fan_control_enabled: true
    fan_control_settings:
      # Fan control mode. Can be either `curve` or `static`
      mode: curve
      # Static fan speed from 0 to 1. Used when `mode` is `static`
      static_speed: 1.0
      # The temperature sensor name to be used with a custom fan curve.
      # This can be used to base the fan curve off  the`junction` (hotspot) 
      # temperature instead of the default overall ("edge") tempreature.
      # Applicable on most Vega and newer AMD GPUs.
      temperature_key: edge
      # Interval in milliseconds for how often the GPU temperature should be checked
      # when adjusting the fan curve.
      interval_ms: 500
      # Custom fan curve used with `mode` set to `curve`.
      # The format of the map is temperature to fan speed from 0 to 1.
      # Note: on RDNA3+ AMD GPUs this must have 5 entries.
      curve:
        40: 0.2
        50: 0.35
        60: 0.5
        70: 0.75
        80: 1.0
      # Hysteresis setting: when spinning down fans after a temperature drop,
      # the target speed needs to be lower for at least this many milliseconds
      # for the fan to actually slow down.
      # This lets you avoid fan speed jumping around during short drops of load
      # (e.g. loading screen in a game).
      spindown_delay_ms: 0
      # Hysteresis setting: the minimum temperature change in degrees 
      # to affect the fan speed. Also used to avoid rapid fan speed changes
      # when the temperature only changes e.g. 1 degree.
      change_threshold: 0
    # Power management firmware options. Specific to RDNA3+ AMD GPUs.
    # Most of these settings are only applied when not using a custom fan curve.
    pmfw_options: 
      # This setting adjusts the PMFW’s behavior about the maximum speed in RPM the fan can spin.
      acoustic_limit: 3200
      # This setting adjusts the PMFW’s behavior about the maximum speed in RPM the fan can spin 
      # when the temperature is not greater than target temperature.
      acoustic_target: 1450
      # The minimum speed in RPM that the fan can spin at.
      minimum_pwm: 15
      # Target temperature for the GPU in degrees.
      # Paring with the acoustic_target setting, they define the maximum speed in RPM 
      # the fan can spin when the temperature is not greater than target temperature. 
      target_temperature: 83
      # When set to `true`, allows the fan to be turned turned off when below the
      # `zero_rpm_threshold` temperature value.
      zero_rpm: true
      # Temperature in degrees below which the fan should be turned off when `zero_rpm` is set to true.
      zero_rpm_threshold: 50
    # Power limit in watts.
    power_cap: 320.0
    # Performance level option for AMD GPUs.
    # Can be `auto`, `low`, `high` or `manual`.
    performance_level: auto
    # Index of an AMD power profile mode.
    # Setting this requires `performance_level` to be set to `manual`.
    power_profile_mode_index: 0
    # Custom heuristic values when using the custom AMD power profile mode.
    # The meaning of these values, their format and count depend on the specific GPU model.
    # Check the names of these values in the UI.
    custom_power_profile_mode_hueristics:
    - - 0
      - 5
      - 1
      - 0
      - 4
      - 800
      - 4587520
      - -65536
      - 0
    - - 0
      - 5
      - 1
      - 0
      - 1
      - 0
      - 3276800
      - -65536
      - -6553
    - - 0
      - 5
      - 1
      - 0
      - 4
      - 800
      - 327680
      - -65536
      - 0
    # List of AMD power states which should be enabled
    power_states:
      # GPU power states
      core_clock:
        - 0
        - 2
        - 3
      # VRAM power states
      memory_clock:
        - 0
        - 1
    
    # Minimum GPU clockspeed in MHz. Applicable to AMD and Intel.
    min_core_clock: 300
    # Minimum VRAM clockspeed in MHz. Applicable to AMD only.
    min_memory_clock: 500
    # Minimum GPU voltage in mV. Applicable to AMD only.
    min_voltage: 900
    # Maximum GPU clockspeed in MHz. Applicable to AMD and Intel.
    max_core_clock: 1630
    # Maximum VRAM clockspeed in MHz. Applicable to AMD only.
    max_memory_clock: 800
    # Maximum GPU voltage in mV.
    max_voltage: 1200
    # Voltage offset value in mV for RDNA and newer AMD GPUs.
    voltage_offset: 0
    
    # GPU and VRAM clockspeed offset values, per-pstate. Only applicable on Nvidia.
    gpu_clock_offsets:
      0: -100
    mem_clock_offsets:
      0: 200

# Settings profiles
profiles:
  # Name of the profile
  vkcube:
    # GPU settings in this profile. 
    # It is the same config format that is used for the top-level `gpus` option.
    gpus: {}
    # Profile activation rule for when this profile shoule be activated 
    # when using automatic profile switching.
    rule:
      # Type of the rule. Can be either `process or `gamemode`.
      type: process
      # Process filter. This is not required when using the gamemode rule type.
      filter:
        # Name of the process.
        name: vkcube
        # Process arguments. Not required.
        args: --my-arg

# Current profile to be used. Does not have effect when `auto_switch_profiles` is used.
# Omit this option or set to `null` to use the default profile (settings in the top-level `gpus` entry).
current_profile: vkcube
# If profiles should be switched between automatically based on their configured rules.
auto_switch_profiles: true
```
