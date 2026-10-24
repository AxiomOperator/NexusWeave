from fastapi import FastAPI
from app import items

api = FastAPI()
api.include_router(items.router, prefix="/api")
