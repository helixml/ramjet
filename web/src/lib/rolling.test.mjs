import assert from "node:assert/strict"
import test from "node:test"

import { rollingAverage } from "./rolling.ts"

test("an expired busy window does not publish floating-point residue", () => {
  // These ordinary percentage/rate pairs leave a -44% quotient if the
  // incrementally added sums are trusted after every observation expires.
  const observations = [
    [87.9127532010898, 100128.84128279984],
    [58.14304968807846, 84388.84608820081],
    [42.64175973366946, 99727.4834420532],
    [87.98318088520318, 28769.740112125874],
    [50.26693360414356, 92264.23434354365],
    [23.27748427633196, 79121.85874208808],
    [10.529471444897354, 93063.61505202949],
    [12.451276672072709, 85909.37910974026],
    [7.869437686167657, 100371.36550433934],
    [45.7729077199474, 73829.34272661805],
    [85.30882119666785, 90209.18083749712],
    [35.340332170017064, 91012.09493726492],
    [5.932248174212873, 99998.97896684706],
  ]
  const rows = observations.map(([hit, prompt_tps], index) => ({
    t: index * 1_000,
    hit,
    prompt_tps,
  }))
  rows.push({ t: 73_000, hit: null, prompt_tps: null })

  const rolled = rollingAverage(rows, {
    key: "hit",
    weightKey: "prompt_tps",
    windowMs: 60_000,
    outKey: "average",
  })

  assert.equal(rolled.at(-1).average, null)
})
