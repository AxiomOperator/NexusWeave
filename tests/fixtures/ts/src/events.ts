import { EventEmitter } from "events";

export const bus = new EventEmitter();

function onUserCreated(id: string) { console.log(id); }
function audit(id: string) { console.log("audit", id); }

bus.on("user:created", onUserCreated);
bus.on("user:created", audit);

export function createUser(id: string) {
  bus.emit("user:created", id);
}
