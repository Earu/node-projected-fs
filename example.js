const { FuseFS } = require('./index.js');

async function main() {
	const fs = new FuseFS();
	
	try {
		// Add some virtual files before mounting
		await fs.addFile("hello.txt", Buffer.from("Hello, World!\n"));
		await fs.addFile("data.bin", Buffer.from([1, 2, 3, 4, 5]));
		await fs.addDirectory("subdir");
		await fs.addFile("subdir/test.txt", Buffer.from("Test file in subdir\n"));

		// Mount the filesystem
		await fs.mount(process.env.HOME + '/fuse-mount');
		console.log('Filesystem mounted at ' + process.env.HOME + '/fuse-mount');
		
		// You can add/remove files while mounted
		setTimeout(async () => {
			console.log('Adding dynamic.txt');
			await fs.addFile("dynamic.txt", Buffer.from("Added while mounted!\n"));
			console.log('Added dynamic.txt');
		}, 5000);

		// Keep the process running until explicitly terminated
		await new Promise((resolve) => {
			process.on('SIGINT', async () => {
				console.log('Received SIGINT signal');
				await fs.unmount();
				console.log('Filesystem unmounted');
				resolve();
			});

			process.on('SIGTERM', async () => {
				console.log('Received SIGTERM signal');
				await fs.unmount();
				console.log('Filesystem unmounted');
				resolve();
			});
		});

		process.exit(0);
	} catch (error) {
		console.error('Error:', error);
		process.exit(1);
	}
}

main(); 