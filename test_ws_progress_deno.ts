// Track active jobs and their progress
const activeJobs = new Map<string, { filename: string; progress: number; message: string; fallback: number; completed: boolean; completionMsg?: string; status: 'queued' | 'running' | 'completed' }>();
let lastRenderTime = 0;
const RENDER_INTERVAL = 100; // Throttle rendering to every 100ms
let displayedJobs: string[] = []; // Only jobs that have started (not queued)

// Concurrency control
const MAX_CONCURRENT = 5;
let runningCount = 0;
const jobQueue: Array<() => Promise<void>> = [];

// Connect to WebSocket for progress updates
const ws = new WebSocket('ws://127.0.0.1:3000/ws-progress');

ws.onopen = () => {
  console.log('WebSocket connected, waiting for progress updates...\n');
};

ws.onmessage = (event) => {
  const progress = JSON.parse(event.data);

  // Add to displayed jobs when first progress update arrives
  if (!displayedJobs.includes(progress.job_id)) {
    displayedJobs.push(progress.job_id);
    // Reserve space for new progress bar
    Deno.stdout.writeSync(new TextEncoder().encode('\n'));
  }

  // Update job tracking (only if not completed)
  const existing = activeJobs.get(progress.job_id);
  if (!existing || !existing.completed) {
    activeJobs.set(progress.job_id, {
      filename: progress.filename,
      progress: progress.progress,
      message: progress.message,
      fallback: progress.fallback_level,
      completed: false,
      status: 'running'
    });
  }

  // Throttle rendering
  const now = Date.now();
  if (now - lastRenderTime > RENDER_INTERVAL) {
    renderProgress();
    lastRenderTime = now;
  }
};

ws.onerror = (error) => {
  console.error('WebSocket error:', error);
};

ws.onclose = () => {
  console.log('WebSocket connection closed');
};

function renderProgress() {
  if (displayedJobs.length === 0) return;

  // Move cursor to beginning of all progress bars
  const moveUp = displayedJobs.length > 1 ? `\x1b[${displayedJobs.length - 1}A` : '';

  const lines = displayedJobs.map(jobId => {
    const job = activeJobs.get(jobId);
    if (!job) return '\x1b[2K\r';

    if (job.completed && job.completionMsg) {
      // Show completion message
      return `\x1b[2K${job.completionMsg}\r`;
    }

    const barWidth = 40;
    const filled = Math.floor(job.progress * barWidth);
    const empty = barWidth - filled;
    const bar = '█'.repeat(filled) + '░'.repeat(empty);
    const percent = (job.progress * 100).toFixed(1).padStart(5);

    const fallbackIndicator = job.fallback === 0 ? '⚡' : job.fallback === 1 ? '🔑' : '🐌';
    const shortFilename = job.filename.length > 30 ? '...' + job.filename.slice(-27) : job.filename.padEnd(30);

    return `\x1b[2K${fallbackIndicator} [${bar}] ${percent}% | ${shortFilename}\r`;
  }).join('\n');

  Deno.stdout.writeSync(new TextEncoder().encode(moveUp + lines));
}

async function processNextJob() {
  if (jobQueue.length === 0 || runningCount >= MAX_CONCURRENT) return;

  const job = jobQueue.shift();
  if (job) {
    runningCount++;
    await job();
    runningCount--;
    processNextJob(); // Start next job
  }
}

function queueJob(filename: string, outputFilename: string, customJobId: string, forceFallback?: number) {
  // Add job to tracking as queued (but not displayed yet)
  activeJobs.set(customJobId, {
    filename,
    progress: 0,
    message: 'Queued',
    fallback: 0,
    completed: false,
    status: 'queued'
  });

  // Add to execution queue
  jobQueue.push(async () => {
    await executeJob(filename, outputFilename, customJobId, forceFallback);
  });

  processNextJob();
}

// Video file extensions to look for
const VIDEO_EXTENSIONS = ['.mp4', '.mkv', '.avi', '.mov', '.wmv', '.flv', '.webm', '.m4v', '.mpg', '.mpeg', '.rm', '.rmvb', '.3gp', '.ogv', '.ogg', '.ogm', '.rl'];

// Generate unique job ID
let jobIdCounter = 0;
function generateJobId(): string {
  return `job-${Date.now()}-${jobIdCounter++}`;
}

// Format milliseconds to human-readable duration
function formatDuration(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);

  const s = seconds % 60;
  const m = minutes % 60;
  const h = hours;

  if (h > 0) {
    return `${h}h ${m}m ${s}s`;
  } else if (m > 0) {
    return `${m}m ${s}s`;
  } else {
    return `${s}s`;
  }
}

// Recursively find all video files in a directory
async function findVideoFiles(dir: string): Promise<string[]> {
  const videos: string[] = [];

  try {
    for await (const entry of Deno.readDir(dir)) {
      const fullPath = `${dir}\\${entry.name}`;

      if (entry.isDirectory) {
        // Recursively search subdirectories
        const subVideos = await findVideoFiles(fullPath);
        videos.push(...subVideos);
      } else if (entry.isFile) {
        // Check if file has video extension
        const ext = entry.name.substring(entry.name.lastIndexOf('.')).toLowerCase();
        if (VIDEO_EXTENSIONS.includes(ext)) {
          videos.push(fullPath);
        }
      }
    }
  } catch (error) {
    console.error(`Error reading directory ${dir}:`, error);
  }

  return videos;
}

async function executeJob(filename: string, outputFilename: string, customJobId: string, forceFallback?: number) {
  const requestBody: any = {
    path: filename,
    render_timestamps: true,
    ...(forceFallback !== undefined && { force_fallback: forceFallback }),
    collage: {
      mode: "grid",
      rows: 5,
      cols: 5,
      width: 1440,
      height: 1080,
      // spacing: 2,
      // margin: 2,
      // background: {"r": 255, "g": 0, "b": 0, "a": 255}
    },
    skip_blank_frames: true,
    // force_fallback: 1,
    selection: {
      start_offset: 10.0,
      end_offset: 90.0,
      mode: "uniform",
      frames: 25
    }
  };

  // Add custom job_id if provided
  if (customJobId) {
    requestBody.job_id = customJobId;
  }

  // Mark as running
  const job = activeJobs.get(customJobId);
  if (job) {
    job.status = 'running';
    renderProgress();
  }

  const now = Date.now();

  const response = await fetch('http://127.0.0.1:3000/make_collage', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json'
    },
    body: JSON.stringify(requestBody),
  });

  if (!response.ok) {
    const text = await response.text();
    const job = activeJobs.get(customJobId);
    if (job) {
      job.completed = true;
      job.status = 'completed';
      job.completionMsg = `✗ ${filename} - Error: ${text}`;
      renderProgress();
    }
    return;
  }

  const buffer = await response.arrayBuffer();
  const timeTaken = Date.now() - now;

  // Save to file
  await Deno.writeFile(outputFilename, new Uint8Array(buffer));

  // Mark job as completed
  const jobData = activeJobs.get(customJobId);
  if (jobData) {
    jobData.completed = true;
    jobData.status = 'completed';
    jobData.completionMsg = `✓ ${outputFilename} (${(buffer.byteLength / 1024).toFixed(1)} KB, ${timeTaken}ms)`;
    renderProgress();
  }
}

// Main function
const main = async () => {
  // Get folder path from command line arguments
  const args = Deno.args;
  if (args.length === 0) {
    console.error('Usage: deno run --allow-net --allow-read --allow-write test_ws_progress_deno.ts <folder_path> [output_dir]');
    console.error('Example: deno run --allow-net --allow-read --allow-write test_ws_progress_deno.ts C:\\Videos C:\\Outputs');
    Deno.exit(1);
  }

  const inputFolder = args[0];
  const outputDir = args[1] || './collages';

  // Create output directory if it doesn't exist
  try {
    await Deno.mkdir(outputDir, { recursive: true });
  } catch (error) {
    // Directory might already exist
  }

  // Wait a bit to ensure WebSocket is connected
  await new Promise(resolve => setTimeout(resolve, 1000));

  console.log(`>>> Scanning folder: ${inputFolder}\n`);
  const now = Date.now();

  // Find all video files
  const videoFiles = await findVideoFiles(inputFolder);

  if (videoFiles.length === 0) {
    console.log('No video files found!');
    Deno.exit(0);
  }

  console.log(`Found ${videoFiles.length} video file(s)\n`);

  // Queue all video files
  for (const videoPath of videoFiles) {
    const filename = videoPath.substring(videoPath.lastIndexOf('\\') + 1);
    const baseFilename = filename.substring(0, filename.lastIndexOf('.'));
    const outputFilename = `${outputDir}\\${baseFilename}_collage.jpg`;
    const jobId = generateJobId();

    queueJob(videoPath, outputFilename, jobId);
  }

  // Wait for all to complete by checking if all jobs are done
  const checkComplete = setInterval(() => {
    const allJobIds = Array.from(activeJobs.keys());
    const allComplete = allJobIds.every(id => activeJobs.get(id)?.completed);
    if (allComplete && allJobIds.length === videoFiles.length) {
      clearInterval(checkComplete);
      const totalTime = Date.now() - now;
      console.log('\n\nTotal time:', formatDuration(totalTime));
      console.log(`>>> All ${videoFiles.length} video(s) processed!`);
      Deno.exit(0);
    }
  }, 500);
};

main().catch(console.error);
