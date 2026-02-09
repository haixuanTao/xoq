#!/bin/bash
# Download OpenArm STL meshes from openarm.dev
BASE="https://openarm.dev/urdf/openarm_v1_0/meshes"
DIR="$(dirname "$0")/meshes"

mkdir -p "$DIR/arm/v10/visual" "$DIR/body/v10/visual" "$DIR/ee/openarm_hand/visual"

for i in 0 1 2 3 4 5 6 7; do
  curl -sL "$BASE/arm/v10/visual/link${i}.stl" -o "$DIR/arm/v10/visual/link${i}.stl" &
done
curl -sL "$BASE/body/v10/visual/body_link0.stl" -o "$DIR/body/v10/visual/body_link0.stl" &
curl -sL "$BASE/ee/openarm_hand/visual/hand.stl" -o "$DIR/ee/openarm_hand/visual/hand.stl" &
curl -sL "$BASE/ee/openarm_hand/visual/finger.stl" -o "$DIR/ee/openarm_hand/visual/finger.stl" &
wait
echo "Downloaded $(find "$DIR" -name '*.stl' | wc -l) mesh files"
