export class User {
  constructor(public name: string) {}
  get label(): string { return this.name; }
  greet(): string { return "hi " + this.name; }
}
export interface Repo { find(id: string): User; }
