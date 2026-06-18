//! Schedule computation and the scheduler loop: next-run calculation,
//! wall-clock alignment, occurrence identity, missed-run detection, and
//! clock-jump/DST handling. Emits run intents; never spawns processes.
