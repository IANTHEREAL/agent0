# pg-tikv Cloud Admin Portal - Backend Dockerfile

FROM python:3.11-slim

WORKDIR /app

# Install Python dependencies (from pyproject.toml)
COPY pyproject.toml .
COPY uv.lock .
COPY app/ ./app/
RUN pip install --no-cache-dir .

# Create non-root user
RUN useradd -m -u 1000 appuser && chown -R appuser:appuser /app
USER appuser

# Expose port
EXPOSE 8080

# Run the application
CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8080"]
