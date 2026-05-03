async function main() {
  const a = await Aster.read("counters/a", "value");
  const b = await Aster.read("counters/b", "value");
  return a + b;
}
