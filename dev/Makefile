COMPOSE := docker compose -f compose.yml

.PHONY: up down logs purge seed reseed build

build:
	$(COMPOSE) build

up:
	$(COMPOSE) up -d --build --wait

down:
	$(COMPOSE) down

logs:
	$(COMPOSE) logs -f

seed:
	$(COMPOSE) --profile seed run --rm seed

# wipes the postgis volume first so the next seed starts from a clean DB.
reseed:
	$(COMPOSE) stop postgis compiler runtime viewer
	docker volume rm -f mars-dev_postgis-data
	$(COMPOSE) up -d --wait postgis
	$(COMPOSE) --profile seed run --rm seed
	$(COMPOSE) up -d --wait

purge:
	$(COMPOSE) down -v --rmi local
