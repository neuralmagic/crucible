**Keep `goal.md` de-prescribed.** Most fixes here are to the gate or the controls, not the goal —
but if you do touch `goal.md`, it stays a *problem* statement: the behavioral symptom, the
measurement, the threshold, the constraints, and **never** the file/function to edit or the
approach that fixes it, even where the upstream issue prescribes one. A *different* agent solves
this autonomously against your gate; a `goal.md` that leaks the fix measures transcription, not
capability. Your reference fix (the `good_cmd` patch under `_controls/`) is private calibration —
it must not bleed into `goal.md`. If the goal as drafted names the fix, the correct move is to
strip the prescription out of `goal.md`, never to weaken the gate.
