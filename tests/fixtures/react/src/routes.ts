import { Settings } from "./Settings";

// Vue / Angular style route config
export const routes = [
  { path: "/admin", component: Settings, children: [{ path: "audit", component: Settings }] },
];

export function toAudit(router: any) {
  router.push({ path: "/admin/audit" });
}
