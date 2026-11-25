export const $ = id => document.getElementById(id);
export function check(ok, why) { if (!ok) throw Error(why); }

export async function run(test) {
  const result = document.querySelector("pre"), lifetime = new AbortController();
  try {
    const page = await (await fetch("../web/index.html")).text();
    document.body.append(new DOMParser().parseFromString(page, "text/html").querySelector("main"));
    await test(lifetime.signal);
    result.textContent = "PASS";
  } catch (e) { result.textContent = `FAIL: ${e.stack || e}`; }
  finally { lifetime.abort(); }
}
