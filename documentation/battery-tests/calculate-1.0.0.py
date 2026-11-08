#!/usr/bin/env python3
"""Recalculates the 1.0.0 battery comparison: RetroAchievements Off against On (Phase 6, Chunk 3).

Run from the repository root:  python3 documentation/battery-tests/calculate-1.0.0.py

The inputs are the summary values from the reviewed Phase 6 results (8 November 2026), whose raw
gauge logs are kept in the project's working folder, not in this repository: each run's
estimated current and its standard error, worked out by the Phase 0B method (timed 1% gauge
steps of 35 mAh, fitted over the window), and the CPU figures read from /proc during each run.

Method (as 0.5.0's Phase 0B): Pokemon Emerald on mGBA in its intro loop, lid open, no input,
unplugged, brightness 5, volume 25, a 40-minute window from the first gauge step at or below 80%.
Both runs covered 80% to 71%. Both had Wi-Fi On (the plan said Off): the RA On run was taken with
Wi-Fi On, and the RA Off run was matched to it rather than repeating it. The comparison is like
for like; the absolute currents are not comparable with 0.5.0's figures (78% to 53%).

This is an active-play measurement. Nothing here is a sleep measurement: 1.0.0's sleep figures
are 0.5.0's, because the sleep implementation did not change.
"""
from math import sqrt

CAPACITY_MAH = 3500

# name: (current mA, standard error mA, game CPU jiffies/s, all CPUs busy %)
RUNS = {
    "RA Off": (494, 5, 56.0, 14.6),
    "RA On (tracking 197 achievements)": (503, 3, 68.3, 17.7),
}

off, on = RUNS["RA Off"], RUNS["RA On (tracking 197 achievements)"]
for name, (ma, se, cpu, busy) in RUNS.items():
    print(f"{name:36} {ma} mA (SE {se}), game CPU {cpu} jiffies/s, all CPUs busy {busy}%")

diff = on[0] - off[0]
diff_se = sqrt(off[1] ** 2 + on[1] ** 2)
print()
print(f"Difference: +{diff} mA, +{100 * diff / off[0]:.1f}%, SE {diff_se:.1f} ({diff / diff_se:.1f} SE)")
# The agreed criterion: within measurement noise if no more than twice its standard error.
print("Verdict:", "within measurement noise" if diff <= 2 * diff_se else "a measurable difference")
print(f"Game CPU: +{on[2] - off[2]:.1f} jiffies/s (+{100 * (on[2] - off[2]) / off[2]:.0f}%); "
      f"all CPUs busy +{on[3] - off[3]:.1f} points")

# What the observed difference would mean over a full charge (projected, not observed).
hours_off = CAPACITY_MAH / off[0]
hours_on = CAPACITY_MAH / on[0]
print(f"Projected play from full at these currents: {hours_off:.2f} h and {hours_on:.2f} h, "
      f"{60 * (hours_off - hours_on):.0f} min apart (rough; ignores the rise in current as the battery empties)")
