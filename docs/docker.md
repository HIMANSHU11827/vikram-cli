# Docker

## Build

```sh
docker build -t vikram:latest .
```

## Run (interactive TUI)

```sh
docker run -it --rm \
  -e OPENAI_API_KEY=$OPENAI_API_KEY \
  -v vikram-data:/data/vikram \
  -v "$PWD":/workspace \
  vikram:latest
```

## Run headless

```sh
docker run --rm \
  -e OPENAI_API_KEY=$OPENAI_API_KEY \
  -v vikram-data:/data/vikram \
  vikram:latest chat "explain this repo"
```

## docker-compose

```sh
OPENAI_API_KEY=sk-... docker compose up
```

State persists in the `vikram-data` volume (`VIKRAM_HOME=/data/vikram`).