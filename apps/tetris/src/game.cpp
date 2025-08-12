#include <cstdio>
#include "game.h"
#include "tetromino.h"

void Game::render() {
    tetromino.updateBoard();

    // 清屏 + 光标回到左上角
    printf("\033[2J\033[H");

    for (int i = 0; i < 20; ++i) {
        for (int j = 0; j < 10; ++j) {
            int colorRendered = board[i][j] + tetromino.board[i+4][j];
            char c = ' ';
            switch (colorRendered) {
                case 1: c = '#'; break;
                case 2: c = '@'; break;
                case 3: c = '*'; break;
                default: c = ' '; break;
            }
            printf("%c%c", c, c);
        }
        printf("\n");
    }

    printf("\nLevel: %d\nScore: %d\n", level + 1, score);

    printf("Next:\n");
    for (int i = 1; i < 5; ++i) {
        for (int j = 3; j < 7; ++j) {
            int colorRendered = nextTetromino.board[i][j];
            char c = (colorRendered != 0) ? '#' : ' ';
            printf("%c%c", c, c);
        }
        printf("\n");
    }

    fflush(stdout);
}

void Game::updateState () {
    // check collisions with the bottom border
    bool collide = !tetromino.moveDown();
    // check collisions with other tetrominoes
    if (collideWithTetrominoes()) {
        tetromino.moveUp();
        collide = true;
    }

    // fix tetromino, update score and spawn a new tetromino
    if (collide){
        tetromino.updateBoard();
        for (int i = 0; i < 20; ++i) {
            for (int j = 0; j < 10; ++j) {
                if (board[i][j] == 0) {
                    board[i][j] = tetromino.board[i+4][j];
                }
            }
        }
        updateScore();
        tetromino = nextTetromino;
        nextTetromino = Tetromino();
    }
}

void Game::updateScore() {
    int rowCleared = 0;
    for (int i = 0; i < 20; ++i) {
        if (isRowCompleted(i)) {
            deleteRow(i);
            rowCleared += 1;
        }
    }

    // Original Nintendo scoring system
    switch (rowCleared) {
        case 1:
            score += 40 * (level + 1);
            break;
        case 2:
            score += 100 * (level + 1);
            break;
        case 3:
            score += 300 * (level + 1);
            break;
        case 4:
            score += 1200 * (level + 1);
            break;
    };

    // level up
    if (completedRows % 10 > 9 && level < 9) level += 1;
}

bool Game::isRowCompleted(int row) {
    for (int j = 0; j < 10; ++j) {
        if (board[row][j] ==  0) return false;
    }
    completedRows += 1;
    return true;
}

void Game::deleteRow(int row) {
    for (int i = row; i > 0; --i) {
        for (int j = 0; j < 10; ++j) {
           board[i][j] = board[i - 1][j];
        }
    }
    for (int j = 0; j < 10; ++j) {
        board[0][j] = 0;
    }
}

bool Game::collideWithTetrominoes() {
    tetromino.updateBoard();
    for (int i = 0; i < 20; ++i) {
        for (int j = 0; j < 10; ++j) {
            if (board[i][j] != 0 && tetromino.board[i+4][j] != 0) {
                return true;
            }
        }
    }
    return false;
}

void Game::trasformTetromino(int key) {
    switch (key) {
        case 'w': // 旋转
            tetromino.rotate();
            if (collideWithTetrominoes()) tetromino.rotate(true);
            break;
        case 'd': // 右移
            tetromino.moveRight();
            if (collideWithTetrominoes()) tetromino.moveLeft();
            break;
        case 'a': // 左移
            tetromino.moveLeft();
            if (collideWithTetrominoes()) tetromino.moveRight();
            break;
        case 's': // 下移
            tetromino.moveDown();
            if (collideWithTetrominoes()) tetromino.moveUp();
            break;
    }
}
