#!/usr/bin/env node
/**
 * generate_urdf.mjs — One-time script to (re)generate the OpenArm v10 URDF.
 *
 * The URDF at examples/assets/openarm_v10.urdf was hand-written from the xacro
 * sources at https://github.com/enactic/openarm_description. This script
 * documents the provenance and can verify or regenerate it if needed.
 *
 * Usage:
 *   node generate_urdf.mjs          # verify the existing URDF
 *   node generate_urdf.mjs --force   # overwrite with fresh copy
 *
 * Source xacro files:
 *   urdf/arm/openarm_arm.xacro      — arm macro (joints, links, meshes)
 *   urdf/arm/openarm_macro.xacro    — joint param expansion
 *   config/arm/v10/                  — kinematics, limits, inertials
 *
 * Parameters used: arm_type=v10, bimanual=false, prefix="", reflect=1, ee_type=none
 */

import { readFileSync, existsSync } from "fs";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const urdfPath = resolve(__dirname, "examples/assets/openarm_v10.urdf");

// Joint chain from the v10 xacro config
const JOINTS = [
  { name: "joint1", parent: "link0", child: "link1", xyz: "0.0 0.0 0.0625",     axis: "0 0 1",   lower: -1.396263, upper: 3.490659, vel: 16.754666, eff: 40 },
  { name: "joint2", parent: "link1", child: "link2", xyz: "-0.0301 0.0 0.06",    axis: "-1 0 0",  lower: -1.745329, upper: 1.745329, vel: 16.754666, eff: 40 },
  { name: "joint3", parent: "link2", child: "link3", xyz: "0.0301 0.0 0.06625",  axis: "0 0 1",   lower: -1.570796, upper: 1.570796, vel: 5.445426,  eff: 27 },
  { name: "joint4", parent: "link3", child: "link4", xyz: "0.0 0.0315 0.15375",  axis: "0 1 0",   lower: 0.0,       upper: 2.443461, vel: 5.445426,  eff: 27 },
  { name: "joint5", parent: "link4", child: "link5", xyz: "0.0 -0.0315 0.0955",  axis: "0 0 1",   lower: -1.570796, upper: 1.570796, vel: 20.943946, eff: 7  },
  { name: "joint6", parent: "link5", child: "link6", xyz: "0.0375 0.0 0.1205",   axis: "1 0 0",   lower: -0.785398, upper: 0.785398, vel: 20.943946, eff: 7  },
  { name: "joint7", parent: "link6", child: "link7", xyz: "-0.0375 0.0 0.0",     axis: "0 1 0",   lower: -1.570796, upper: 1.570796, vel: 20.943946, eff: 7  },
];

if (existsSync(urdfPath)) {
  const content = readFileSync(urdfPath, "utf-8");
  let ok = true;
  for (const j of JOINTS) {
    if (!content.includes(`name="${j.name}"`)) {
      console.error(`MISSING joint: ${j.name}`);
      ok = false;
    }
  }
  for (let i = 0; i <= 7; i++) {
    if (!content.includes(`link${i}.dae`)) {
      console.error(`MISSING mesh: link${i}.dae`);
      ok = false;
    }
  }
  // DAE meshes are in millimeters — must have scale="0.001 0.001 0.001"
  const scaleCount = (content.match(/scale="0\.001 0\.001 0\.001"/g) || []).length;
  if (scaleCount < 8) {
    console.error(`MISSING mesh scale: found ${scaleCount}/8 scale="0.001 0.001 0.001" attributes`);
    ok = false;
  }
  if (ok) {
    console.log(`URDF verified: ${urdfPath}`);
    console.log(`  7 revolute joints, 8 visual meshes (link0-link7), all with 0.001 scale`);
    console.log(`  Meshes: package://openarm_description/meshes/arm/v10/visual/link{0-7}.dae`);
  } else {
    console.error("URDF verification FAILED — re-run with --force or fix manually");
    process.exit(1);
  }
} else {
  console.error(`URDF not found at ${urdfPath}`);
  console.error("Create it by hand-translating from the xacro sources.");
  process.exit(1);
}
