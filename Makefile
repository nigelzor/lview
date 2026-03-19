.PHONY: all
all: sets.csv.gz themes.csv.gz

# from https://rebrickable.com/downloads/
sets.csv.gz:
	curl -o $@ "https://cdn.rebrickable.com/media/downloads/sets.csv.gz?1773817924.764488"

themes.csv.gz:
	curl -o $@ "https://cdn.rebrickable.com/media/downloads/themes.csv.gz?1773817920.06436"

.PHONY: dev
dev: sets.csv.gz themes.csv.gz
	cargo watch -x 'run -- --dir files'
