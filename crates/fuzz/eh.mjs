import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const ARGS = [0, 1, 2, 3, 5, 17, 39, -1, 1000];

function trace(path) {
  const log = [];
  const module = new WebAssembly.Module(readFileSync(path));
  const instance = new WebAssembly.Instance(module, {
    env: { log: (x) => void log.push(x) },
  });
  const ex = instance.exports;
  const tags = Object.entries(ex).filter(([, v]) => v instanceof WebAssembly.Tag);
  const out = [];
  const names = Object.keys(ex).filter((n) => typeof ex[n] === "function").sort();
  for (const name of names) {
    for (const a of ARGS) {
      let r;
      try {
        r = `-> ${ex[name](a)}`;
      } catch (e) {
        if (e instanceof WebAssembly.Exception) {
          const tag = tags.find(([, t]) => e.is(t));
          if (!tag) {
            r = "throw <unexported tag>";
          } else {
            const args = [];
            for (let i = 0; ; i++) {
              try {
                args.push(e.getArg(tag[1], i));
              } catch {
                break;
              }
            }
            r = `throw ${tag[0]}(${args.join(", ")})`;
          }
        } else if (e instanceof RangeError) {
          return null;
        } else {
          r = `trap ${e.constructor.name}: ${e.message}`;
        }
      }
      out.push(`${name}(${a}) ${r} g=${ex.g.value} log=[${log.splice(0).join(",")}]`);
    }
  }
  return out;
}

function compare(input, output) {
  let a, b;
  try {
    a = trace(input);
  } catch (e) {
    return [`input fails to run: ${e}`];
  }
  try {
    b = trace(output);
  } catch (e) {
    return [`output fails to run: ${e}`];
  }
  if (a === null || b === null) return [];
  const at = a.findIndex((x, k) => x !== b[k]);
  if (at < 0 && a.length === b.length) return [];
  return [
    `first difference at call ${at}`,
    `  input:  ${a[at]}`,
    `  output: ${b[at]}`,
  ];
}

for await (const line of createInterface({ input: process.stdin })) {
  const [input, output] = line.split("\t");
  const problems = compare(input, output);
  process.stdout.write(problems.length ? `bad\n${problems.join("\n")}\nend\n` : "ok\n");
}
