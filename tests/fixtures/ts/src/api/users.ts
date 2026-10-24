import { Router } from "express";

const router = Router();

function listUsers(req: any, res: any) { res.json([]); }

router.get("/:id", listUsers);

export default router;
