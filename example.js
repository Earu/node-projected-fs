const { FuseFS } = require('./index.js');

async function main() {
	const fs = new FuseFS();
	
	try {
		// Subscribe to file system events
		fs.on((event) => console.log(`${event.eventType} event:\n- Path: ${event.path}\n- Type: ${event.objectType}`));

		console.log('Creating initial files and directories...');
		
		// Add some virtual files before mounting
		await fs.addFile("hello.txt", Buffer.from("Hello, World!\n"));
		await fs.addFile("data.bin", Buffer.from([1, 2, 3, 4, 5]));
		await fs.addDirectory("subdir");
		await fs.addFile("subdir/test.txt", Buffer.from("Test file in subdir\n"));

		// Mount the filesystem with 100MB RAM allocation
		const RAM_SIZE = 100 * 1024 * 1024; // 100MB in bytes
		await fs.mount(process.env.HOME + '/fuse-mount', RAM_SIZE);
		console.log('Filesystem mounted at ' + process.env.HOME + '/fuse-mount');
		console.log('RAM allocated: ' + (RAM_SIZE / 1024 / 1024) + 'MB');
		
		// Test file operations while mounted
		setTimeout(async () => {
			console.log('\nTesting file operations...');
			
			// Create a new file
			console.log('Adding dynamic.txt...');
			await fs.addFile("dynamic.txt", Buffer.from("Added while mounted!\n"));

			// Create a new directory
			setTimeout(async () => {
				console.log('\nCreating new directory...');
				await fs.addDirectory("newdir");
				
				// Create a file in the new directory
				setTimeout(async () => {
					console.log('\nAdding file in new directory...');
					await fs.addFile("newdir/nested.txt", Buffer.from("Nested file\n"));
					
					// Remove files and directory
					setTimeout(async () => {
						console.log('\nRemoving files and directories...');
						await fs.removePath("hello.txt");
						await fs.removePath("newdir/nested.txt");
						await fs.removePath("newdir");
					}, 2000);
				}, 2000);
			}, 2000);
		}, 2000);

		// Handle graceful shutdown
		process.on('SIGINT', async () => {
			console.log('\nUnmounting filesystem...');
			try {
				await fs.unmount();
				console.log('Filesystem unmounted successfully');
				process.exit(0);
			} catch (error) {
				console.error('Error unmounting:', error);
				process.exit(1);
			}
		});

		console.log('\nPress Ctrl+C to unmount and exit');
		console.log('Try modifying files in the mounted directory to see events...');

		// Keep the process alive
		process.stdin.resume();

	} catch (error) {
		console.error('Error:', error);
		process.exit(1);
	}
}

main(); 