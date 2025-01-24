const { FuseFS } = require('./index.js');
const path = require('path');
const os = require('os');
const fs = require('fs');

function findFiles(dirPath) {
	const results = [];
	const entries = fs.readdirSync(dirPath, { withFileTypes: true });

	for (const entry of entries) {
		const fullPath = path.join(dirPath, entry.name);

		if (entry.isDirectory()) {
			results.push(...findFiles(fullPath));
		} else {
			results.push(fullPath);
		}
	}

	return results;
}

async function main() {
	const fs_impl = new FuseFS();

	try {
		// Subscribe to file system events
		fs_impl.on((event) => console.log(`${event.eventType} event:\n- Path: ${event.path}\n- Type: ${event.objectType}`));

		console.log('Creating initial files and directories...');

		// Add some virtual files before mounting
		await fs_impl.addFile("hello.txt", Buffer.from("Hello, World!\n"));
		await fs_impl.addFile("data.bin", Buffer.from([1, 2, 3, 4, 5]));
		await fs_impl.addDirectory("subdir");
		await fs_impl.addFile("subdir/test.txt", Buffer.from("Test file in subdir\n"));

		// Mount the filesystem with 100MB RAM allocation
		const RAM_SIZE = 100 * 1024 * 1024; // 100MB in bytes
		const mountPath = path.join(os.homedir(), 'projected-fs-mount');

		// Ensure mount point is clean
		if (fs.existsSync(mountPath)) {
			fs.rmSync(mountPath, { recursive: true, force: true });
		}

		// Create a fresh mount directory
		fs.mkdirSync(mountPath, { recursive: true });

		// Now mount with the prepared directory
		await fs_impl.mount(mountPath, RAM_SIZE);
		console.log('Filesystem mounted at ' + mountPath);
		console.log('RAM allocated: ' + (RAM_SIZE / 1024 / 1024) + 'MB');

		// Test file operations while mounted
		setTimeout(async () => {
			console.log('\nTesting file operations...');

			// Create a new file
			console.log('Adding dynamic.txt...');
			await fs_impl.addFile("dynamic.txt", Buffer.from("Added while mounted!\n"));

			// Create a new directory
			setTimeout(async () => {
				console.log('\nCreating new directory...');
				await fs_impl.addDirectory("newdir");

				// Create a file in the new directory
				setTimeout(async () => {
					console.log('\nAdding file in new directory...');
					await fs_impl.addFile("newdir/nested.txt", Buffer.from("Nested file\n"));

					// Remove files and directory
					setTimeout(async () => {
						console.log('\nRemoving files and directories...');
						await fs_impl.removePath("hello.txt");
						await fs_impl.removePath("newdir/nested.txt");
						await fs_impl.removePath("newdir");

						try {
							console.log('\nListing files in mounted directory...');
							const files = findFiles(mountPath);
							console.log(files);
						} catch (error) {
							console.error('Error listing files:', error);
						}
					}, 2000);
				}, 2000);
			}, 2000);
		}, 2000);

		// Handle graceful shutdown
		process.on('SIGINT', async () => {
			console.log('\nUnmounting filesystem...');
			try {
				await fs_impl.unmount();
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