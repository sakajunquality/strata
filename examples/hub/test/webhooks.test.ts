// Where webhooks may go, when the hub is not told that its own network is fair game.
import { expect, test } from "bun:test";

process.env.HUB_DB = ":memory:";
delete process.env.HUB_WEBHOOKS_INSIDE;
const { refusal } = await import("../server/webhooks");

test("webhooks do not reach into the hub's own network", async () => {
  for (const inside of ["http://127.0.0.1:8080/x", "http://localhost/x", "http://10.1.2.3/", "http://192.168.0.1/", "http://172.20.0.1/", "http://169.254.169.254/latest/meta-data", "http://[::1]/", "http://[fd00::1]/", "http://[::ffff:127.0.0.1]/"]) {
    expect(await refusal(inside)).toContain("inside the hub's own network");
  }
  expect(await refusal("ftp://example.com/x")).toContain("http or https");
  expect(await refusal("not an address")).toBe("that is not an address");
  expect(await refusal("https://203.0.113.10/hook")).toBeNull();
  expect(await refusal("http://172.32.0.1/")).toBeNull();
});
