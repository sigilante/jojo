run: jojo.jam
	MINIMAL_LOG_FORMAT=true cargo run

jojo.jam: ./hoon/apps/jojo.hoon
	hoonc --output ./jojo.jam ./hoon/apps/jojo.hoon

clean:
	rm -f jojo.jam
	rm -rf .data.jojo
